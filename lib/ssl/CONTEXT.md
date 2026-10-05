# ssl — TLS/DTLS Application

## Intent
Erlang/OTP `ssl` application (application name `ssl`, `SSL_VSN` = 11.7.7 in `vsn.mk`): a full TLS 1.2/1.3 and DTLS 1.2 client and server implementation, plus the Erlang-distribution-over-TLS transports.
It is one of the `lib/<app>/` OTP applications (built in place, `BUILD_ALL` list in `../Makefile`) and sits directly on `crypto` (primitives), `public_key` (certificates/ASN.1) and `kernel` (sockets/`inet`/`logger`) — it implements no crypto of its own and has no NIF/C code.
Everything the app ships is Erlang: 78 modules (~46k lines) under `src/`, internal `.hrl` headers next to them, Common Test suites in `test/`, published docs in `doc/` and developer notes in `internal_doc/`.

## API Surface
- Public API module: `src/ssl.erl` — `start/0,1`, `stop/0`, `connect/2,3,4`, `listen/2`, `transport_accept/1,2`, `handshake/1,2,3`, `handshake_continue/2,3`, `handshake_cancel/1`, `send/2`, `recv/2,3`, `close/1,2`, `shutdown/2`, `controlling_process/2`, `getopts/2`/`setopts/2`, `peername/1`/`sockname/1`/`peercert/1`, `connection_information/1,2`, `negotiated_protocol/1`, `cipher_suites/2,3` + suite filter/prepend/append, `signature_algs/2`, `versions/0`, `eccs/0,1`, `groups/0,1`, `format_error/1`, `renegotiate/1`, `update_keys/2`, `export_key_materials/4,5`, `clear_pem_cache/0`, `handle_trace/3`.
- Public behaviours users implement: `src/ssl_session_cache_api.erl` (custom session cache) and `src/ssl_crl_cache_api.erl` (custom CRL cache); `src/ssl_crl_cache.erl` is the default CRL cache implementation (also documented as public).
- App resources: `src/ssl.app.src` (module list + deps, `vsn` from `%VSN%`) and `src/ssl.appup.src` (upgrade/downgrade instructions used by `ssl_upgrade_SUITE`).
- `vsn.mk` defines `SSL_VSN`; `Makefile` sets `VSN = $(SSL_VSN)`, `SUB_DIRECTORIES = src doc examples/certs examples/src`, `TEST_NEEDS_RELEASE = true`, `DIA_PLT_APPS = crypto runtime_tools inets public_key`, then includes `../make/app_targets.mk`.
- `include/` and `priv/bin/`, `priv/obj/` are tracked but contain only `.gitignore` — nothing is exported or shipped from them today.
- `ebin/` is build output (only `.gitignore` tracked).

## Constraints
- Allowed application dependencies are exactly `crypto`, `public_key`, `kernel`, `stdlib` (see `src/ssl.app.src`) — never add a dependency on a higher-level OTP app (`inets`, `runtime_tools` etc. appear only as test/tooling runtime deps).
- Internal headers live in `src/` and are included by bare filename; `include/` deliberately exports nothing, so a new public header would be an API change (avoid).
- `ebin/ssl.app` / `ebin/ssl.appup` are generated from the `.src` templates by `%VSN%` substitution — never edit generated outputs, and keep the `Makefile` module lists and `ssl.app.src` in sync when adding/removing a module.
- The build uses `-Werror`; any new compiler warning breaks it.
- Options are validated in one place (`src/ssl_config.erl`); new options must be registered there and in `ssl.erl`'s type/option docs, not handled ad hoc in connection modules.

## Design Decisions
- Layered architecture: public API (`ssl`) → generic connection state machine (`ssl_gen_statem` dispatching to `tls/`dtls`-specific connection modules) → handshake layer (`ssl_handshake` + protocol-specific `tls_handshake`/`tls_handshake_1_3`/`dtls_handshake`) → record layer (`ssl_record` + `tls_record`/`tls_record_1_3`/`dtls_record`) → `crypto`/`public_key`.
- Naming encodes the specialisation: unprefixed `ssl_*` modules hold protocol-independent logic and are specialised *by call* (`tls_*` / `dtls_*`), while `*_1_3` suffixes mark TLS 1.3 modules whose handshake, key schedule and record format differ enough from 1.2 to warrant separate state machines.
- A dedicated `tls_sender` process owns record encryption and socket writes so the receiving state machine never blocks (also required for TLS 1.3 key updates/post-handshake messages).
- The opaque `sslsocket()` record carries a `connection_cb` discriminator (`tls_gen_connection | dtls_gen_connection`) so the `ssl` API can route calls without inspecting the protocol version.
- Erlang distribution over TLS is implemented as pluggable `dist` transport modules in this app (`inet_tls_dist`/`inet6_tls_dist` + their supervision trees); the same app also provides the kTLS path used by the dist and by ordinary TLS sockets.
- DTLS multiplexes many connections over one UDP socket via `dtls_packet_demux`, which also owns "emulated" per-connection socket options.

## Known Issues / Notes for Agents
- Recurring comments in `src/ssl_pem_cache.erl`, `src/ssl_manager.erl` and `test/ssl_pem_cache_SUITE.erl` point at a non-existent `internal_doc/pem_and_cert_cache.md`; that content now lives in `internal_doc/ssl_notes.md`.
- Several huge single files (`ssl_handshake.erl` ~196 KB, `ssl.erl` ~163 KB, `ssl_gen_statem.erl` ~114 KB, `ssl_config.erl` ~95 KB) must be navigated by grep/`-spec`, not read end to end.
- Only 4 `src/` modules carry a real `-moduledoc`; the rest are `-moduledoc false`, i.e. their docs are internal by design — do not "fix" that silently.
- Deliberate `-compile([{nowarn_possibly_unsafe_function, ...}])` suppressions around `list_to_atom`/`binary_to_term` in option/name parsing are intentional; removing them fails the `-Werror` build.
- Most shipped documentation is generated from the `-moduledoc`/`-doc` attributes inside `src/*.erl` (ExDoc reads the compiled `ebin` doc chunks), so API text changes usually belong in the `.erl` file, not in `doc/`.
- The `examples/` tree is excluded from releases at the source location but its PEMs are stale/expired and its installed path differs — see `examples/CONTEXT.md` before touching or advertising the example.

## Dependencies
- Runtime: `crypto`, `public_key`, `kernel`, `stdlib` (declared in `src/ssl.app.src`); `runtime_dependencies` also lists `erts`, `inets`, `runtime_tools` for tooling/tests.
- Build: GNU make + the shared `../make/` infrastructure; the app has no `configure`/autoconf and no C sources.
- Tests: an external OpenSSL CLI on `$PATH` (version-gated; on Windows run through WSL), and `inets`/`httpd` for the CRL distribution-point tests. kTLS cases require a kTLS-capable Linux kernel.

## Test Strategy
- `make test` in this directory builds a temporary release and runs the suites from `test/` through Common Test; `TEST_NEEDS_RELEASE = true` means source or suite changes must be built/released before `ct_run` sees them.
- Single suite: `make ARGS="-suite ssl_cipher_SUITE" test`; default spec is `test/ssl.spec`, CI uses `SPEC_POSTFIX=gh` to pick `test/ssl_gh.spec` (skips benchmarks and `ssl_upgrade_SUITE`).
- Full runs are multi-node, network-bound and drive real OpenSSL peers, so expect hours; most certificate/CRL/key material is generated into `priv_dir` per run by `test/make_certs.erl` and `ssl_test_lib`.
- See `test/CONTEXT.md` for the suite map, helper modules, OpenSSL version gates and the known fixture gotchas.

## Routing Table
| Path | Contents |
| --- | --- |
| `src/` | All 78 Erlang modules and the internal `.hrl` headers: public `ssl` API, connection state machines, handshake/record layers, ciphers, sessions/tickets, cert/CRL handling, supervision, sockets, dist-over-TLS, logging. See `src/CONTEXT.md`. |
| `test/` | Common Test suites (57 `*_SUITE.erl`), helper libs (`ssl_test_lib`, `make_certs`, `ssl_cert_tests`), specs and coverage config. See `test/CONTEXT.md`. |
| `test/property_test/` | QuickCheck/EQC property modules driven by `ssl_eqc_SUITE`. See `test/property_test/CONTEXT.md`. |
| `doc/` | Published documentation: ExDoc config, `ssl_app.md`, the five guides, release notes. See `doc/CONTEXT.md`. |
| `internal_doc/` | Developer notes (`ssl_notes.md`): OCSP stapling model, PEM/cert caches, test-cert topology. See `internal_doc/CONTEXT.md`. |
| `examples/` | Example TLS client/server program and its certificate material, plus sub-nodes `examples/src/`, `examples/certs/`. See `examples/CONTEXT.md`. |
| `include/` | Tracked but empty (`.gitignore` only) — no public headers are exported. |
| `priv/` | `bin/` and `obj/` skeleton dirs, `.gitignore` only; nothing is shipped. |
| `ebin/` | Build output only (`.gitignore`); do not hand-edit. |

## See Also
- `../CONTEXT.md` — how `lib/` is coordinated (`SUB_DIRECTORIES`, build order) and the app directory conventions.
- `../crypto/`, `../public_key/` — the crypto primitives and X.509/ASN.1 implementation `ssl` builds on (read-only cross-references).
- `../kernel/` — socket/`inet`/`logger` APIs and the `dist` infrastructure that drives `inet_tls_dist` (read-only cross-reference).
- `../../make/doc.mk` / `../../make/ex_doc.exs` — how this app's HTML/man docs are generated from `src/` doc chunks and `doc/` markdown.
- `../../HOWTO/TESTING.md` — general Erlang/OTP test instructions behind `make test`.
