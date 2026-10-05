# diameter example nodes (`examples/code`)

## Intent
Small, self-contained example Diameter nodes (client, server, relay agent, redirect agent) plus a raw `gen_sctp` demo, shipped as user-facing tutorial code for the `diameter` application.
Each Diameter example is an *interface module* that starts/stops a service and adds transports, paired with a `diameter_app` *callback module* implementing the Diameter callbacks.
Also serves as compilation/regression coverage that the documented public `diameter` API still works end-to-end.

## API Surface
Per-file:
- `client.erl` — example Diameter client; sends base-protocol ACR requests to a connected peer (`common` application alias).
- `client_cb.erl` — `diameter_app` callback for `client.erl`; `prepare_request/3` fills Origin-Host/Realm, `handle_request/3` answers 3001.
- `server.erl` — example Diameter server; answers ACRs; also exports the transport `message_cb` `message/3` used to bound concurrent requests (default `32`).
- `server_cb.erl` — `diameter_app` callback for `server.erl`; answers `ACR` with `'ACA'` `Result-Code` 2001, otherwise `answer-message` 3001.
- `relay.erl` — example relay agent; `listen/2` delegates to `server:listen/2`, `connect/2` to `client:connect/2` (single service used for both directions).
- `relay_cb.erl` — `diameter_app` callback for `relay.erl`; relays incoming requests (`{relay, [{timeout, 2000}, ...]}`) and answers.
- `redirect.erl` — example redirect agent; `listen/2` delegates to `server:listen/2`.
- `redirect_cb.erl` — `diameter_app` callback for `redirect.erl`; always replies with an `answer-message` carrying `Result-Code` 3006 (`DIAMETER_REDIRECT_INDICATION`) and a `'Redirect-Host'` AVP, built via local `?Dict = diameter_gen_base_rfc6733`.
- `sctp.erl` — standalone `gen_sctp` association demo (`assoc/0`, `dbg/0`), unrelated to the `diameter_app` behaviour.
- `GNUmakefile` — convenience build (`all`/`clean`) compiling the 8 example modules with `erlc -W`.
- `README` — human-facing tutorial: the manual Erlang-shell walkthrough and the list of recommended service options.

Interface-module exports (service name defaults to the module name):
- `client` — `start/0,1,2`, `connect/1,2`, `call/0,1,2`, `stop/0,1` (`call/2` sends an ACR built from a map, supplying default `'Destination-Realm'`, `'Accounting-Record-Type'` and `'Accounting-Record-Number'`, plus a generated `'Session-Id'` when absent).
- `server` — `start/0,1,2`, `listen/1,2`, `stop/0,1`, plus internal `message/3`.
- `relay` — `start/0,1,2`, `listen/1,2`, `connect/1,2`, `stop/0,1`.
- `redirect` — `start/0,1,2`, `listen/1,2`, `stop/0,1`.

Callback pairing table (`{module, ...}` in each service config):
| Interface | Callback | Application alias | Dictionary |
| --- | --- | --- | --- |
| `client` | `client_cb` | `common` | `diameter_gen_base_rfc6733` |
| `server` | `server_cb` | `common` | `diameter_gen_base_rfc6733` |
| `relay` | `relay_cb` | `relay` | `diameter_gen_relay` |
| `redirect` | `redirect_cb` | `redirect` | `diameter_gen_relay` |

Every `*_cb` exports the same callback set: `peer_up/3`, `peer_down/3`, `pick_peer/4`, `prepare_request/3`, `prepare_retransmit/3`, `handle_answer/4`, `handle_error/4`, `handle_request/3`.

## Constraints
- The public `diameter` API used here is the contract for the examples: they must keep calling `diameter:start/0`, `start_service/2`, `add_transport/2`, `stop_service/1`, `call/4`, `session_id/1`, `subscribe/1`.
- Every `*.erl` in this directory is compiled by `diameter_examples_SUITE` with `warnings_as_errors` — `sctp.erl` included, even though it is not released — so all files must stay warning-clean on their own.
- Adding or removing files here requires updating `EXAMPLES` in `../../src/modules.mk` (released list) and possibly `../../test/Makefile` `release_tests_spec`.
- Examples must link only against dictionaries shipped in the app (`diameter_gen_base_rfc6733`, `diameter_gen_relay`); they deliberately do NOT use `../dict/*.dia`, so no `.dia` compilation is needed to build or run them.
- Header usage must stay `-include_lib("diameter/include/diameter.hrl")` / `-include_lib("kernel/include/inet_sctp.hrl")` — the test suite relies on `-include_lib` resolution against a synthesized include directory.
- Never change the service names away from atoms: `Origin-Host` defaults derive from the service name.

## Design Decisions
- Interface module + `*_cb` module split mirrors the `diameter_app` behaviour: the interface module holds service configuration, the callback module the protocol logic, as the README explains (a real node may share one callback across several applications).
- Convenience `opts/1` mappers (in `client.erl`, `server.erl` and shared through delegation by `relay.erl`/`redirect.erl`) translate atoms like `tcp`/`sctp`/`any` and tuples like `{tcp, loopback, 3868}` into `{transport_module, _}`/`{transport_config, _}` pairs, so `server:listen(tcp)` / `client:connect(tcp)` work in a bare shell.
- The configured option set (`decode_format map`, `restrict_connections false`, `strict_mbit false`, `string_decode false`, `call_mutates_state false`) is the README's recommended set: it removes compile-time dependencies on generated dictionary records, allows multiple connections per peer, keeps string AVPs as binaries, tolerates the M-bit not listed in message grammars, and avoids serializing callbacks through a single process.
- `redirect_cb` uses `decode_format map`-style messages plus a local `?Dict` macro to encode `answer-message` AVPs, showing how to build a message the configured dictionary doesn't define.
- `relay.erl`/`redirect.erl` reuse `server.erl`/`client.erl`'s transport option handling rather than duplicating it, so the `opts/1` convenience lives in exactly two modules.

## Known Issues / Notes for Agents
- Defaults are hard-coded: `127.0.0.1` (loopback) and port `3868`; `server.erl` also hard-codes `{reuseaddr, true}`/`{sender, true}`.
- SCTP examples require OS SCTP support; where unavailable only TCP paths can be exercised.
- `code/README` is the human-facing walkthrough (manual shell commands plus the recommended-option rationale) and is NOT in the released `EXAMPLES` list; `sctp.erl` is likewise unreleased.
- The README also lists further optional service settings (`pool_size`, `sequence`, `share_peers`/`use_shared_peers`, `spawn_opt`) that the examples do not configure.
- Running by hand: `diameter:start().` then `server:start(). server:listen(tcp). client:start(). client:connect(tcp). client:call().`; a relay can be inserted with `relay:start()/relay:connect(sctp)/relay:listen(tcp)`.
- `GNUmakefile` assumes `erlc` is on `PATH` and compiles standalone (no dependency on a built diameter app or its include path); it is a developer convenience, not part of the OTP build.
- `server:message/3` counts concurrent requests by inspecting the command code bit of the raw binary and is capped at 32.

## Test Strategy
- `../../test/diameter_examples_SUITE.erl` testcase `code` is the enforcement point: it copies the example sources into a temp dir, synthesizes a diameter include dir, and compiles every `*.erl` with `warnings_as_errors` in a separate node.
- It then starts the examples on a node pair — `server:start/0` + `server:listen({Prot, any, 3868})` and `client:start/0` + `client:connect({Prot, loopback, loopback, 3868})`, subscribing to `diameter_event` to wait for the transport to come up — and sends 100 `client:call/0` requests over each of `tcp` and (when the platform has SCTP) `sctp`.
- `relay` and `redirect` are compiled but never executed by the suite: their behaviour is only checked for compilation correctness.
- Tests need an alive/ distributed node; `?PROTS` is `[sctp || ?HAVE_SCTP()] ++ [tcp]`.

## Dependencies
- `erlc` on `PATH` for the standalone `GNUmakefile` build; the real build/test path uses the built `diameter` application.
- OS SCTP support for the SCTP paths (OS-level, not the package manager).

## See Also
- `..` (`examples/`) — non-code example material (dictionaries) in the same examples tree.
- `../src/modules.mk` — release file list containing `EXAMPLES`.
- `../test/diameter_examples_SUITE.erl` — the suite that compiles and runs this directory.
- `../include/diameter.hrl` — the header the callback modules include; `../src/dict/` — sources of the built-in base/relay dictionaries.
