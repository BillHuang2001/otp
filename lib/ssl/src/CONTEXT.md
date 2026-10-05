# ssl — Source (`lib/ssl/src/`)

## Intent
Complete Erlang implementation of the `ssl` application (TLS 1.2/1.3 and DTLS 1.2 client and server) on top of `crypto`, `public_key`, `kernel` and `stdlib`.
Holds all 78 `.erl` modules AND all 16 internal `.hrl` headers — `lib/ssl/include/` exports nothing, so headers are `-include("...hrl")`'d by bare filename from within this directory.
There are no subdirectories; this is a flat directory, so the "routing table" below is a grouped module map rather than a set of child nodes.

## API Surface
Public application resource: `ssl.app.src` (lists every module; `vsn` substituted from `%VSN%` at build time) and `ssl.appup.src` (release upgrade/downgrade instructions).
`Makefile` is the build spec: `MODULES` (75) + `BEHAVIOUR_MODULES` (`ssl_trace`, `ssl_crl_cache_api`, `ssl_session_cache_api`) + `INTERNAL_HRL_FILES`; `release_spec` installs all `.erl` + `.hrl` into `lib/ssl-$(VSN)/src` and the beams into `ebin`.
Only 4 modules carry a real `-moduledoc` (`ssl`, `ssl_session_cache_api`, `ssl_crl_cache_api`, `ssl_crl_cache`); the other 74 are `-moduledoc false` (internal).

### Public API & app structure
- `ssl.erl` — the sole public API module (162 KB): `start/0,1`, `stop/0`, `connect/2,3,4`, `listen/2`, `transport_accept/1,2`, `handshake/1,2,3`, `handshake_continue/2,3`, `handshake_cancel/1`, `send/2`, `recv/2,3`, `close/1,2`, `shutdown/2`, `controlling_process/2`, `peername/1`, `peercert/1`, `sockname/1`, `getopts/2`, `setopts/2`, `getstat/1,2`, `cipher_suites/2,3`, `filter/prepend/append_cipher_suites/2`, `signature_algs/2`, `versions/0`, `eccs/0,1`, `groups/0,1`, `format_error/1`, `renegotiate/1`, `update_keys/2`, `export_key_materials/4,5`, `negotiated_protocol/1`, `connection_information/1,2`, `tls_version/1`, `suite_to_str/1`, `suite_to_openssl_str/1`, `str_to_suite/1`, `handle_trace/3`, `clear_pem_cache/0`.
- `ssl_app.erl` — `application` behaviour; starts `ssl_sup`.
- `ssl_sup.erl`, `ssl_admin_sup.erl`, `ssl_connection_sup.erl` — top-level supervision.
- `ssl_config.erl` (95 KB) — single place that parses/validates/normalises all SSL options and socket options; `init/2`, `handle_options/3,4`, `update_options/3`, `new_emulated/2`.
- `ssl_alert.erl` — alert handling and encoding; alerts are *thrown* out of the connection modules (see `ssl_alert:send_alert/…`).
- `ssl_srp_primes.erl` — SRP prime table (RFC 5054).

### Connection state machine (the core)
`ssl_gen_statem.erl` (114 KB) is the actual `gen_statem`/`proc_lib` process that owns the socket; its `init_statem/1` dispatches on role and `versions` to a per-version connection module.
- Generic / shared:
  - `tls_dtls_gen_connection.erl` — common TLS+DTLS logic for TLS 1.0–1.2 (pre-1.3): `initial_state/9`, `hello/user_hello/abbreviated/certify/cipher/connection/downgrade` state handlers, master-secret & cert handling.
  - `tls_dtls_client_connection.erl` / `tls_dtls_server_connection.erl` — shared client/server pre-1.3 behaviour on top of the above.
- TLS pre-1.3: `tls_gen_connection.erl`, `tls_client_connection.erl`, `tls_server_connection.erl`.
- TLS 1.3: `tls_gen_connection_1_3.erl`, `tls_client_connection_1_3.erl` (53 KB), `tls_server_connection_1_3.erl` (52 KB) — distinct state machines (ClientHello→…→Finished differ from 1.2); selected only when `versions == ['tlsv1.3']`.
- DTLS: `dtls_gen_connection.erl`, `dtls_client_connection.erl`, `dtls_server_connection.erl`.
- `tls_sender.erl` — separate `gen_statem` process that performs the actual record encryption/sending (and TLS 1.3 key updates / post-handshake messages) so the receiver process never blocks on socket writes.

### Handshake layer
- `ssl_handshake.erl` (196 KB — largest file) — protocol-independent handshake crypto/verification helpers (certify, verify_signature, master_secret, extensions, session, handshake history).
- `tls_handshake.erl` — TLS handshake encode/decode + ClientHello/Hello building (RFC 5246).
- `tls_handshake_1_3.erl` (98 KB) — TLS 1.3 handshake: key schedule, extensions, PSK/early-data, HRR, key updates; `hs_traffic_secrets/2`, `early_data_secret/1`.
- `dtls_handshake.erl` — DTLS handshake framing (fragmentation, sequence numbers).

### Record layer
- `ssl_record.erl` — generic connection-state (security parameter) bookkeeping: `initial_security_params/1`, `current/pending/activate_pending_connection_state`, `set_security_params/3`, `set_mac_secret/4`, plus AEAD helpers.
- `tls_record.erl` — TLS record encode/decode + protocol-version helpers (`protocol_version/1`, `highest/lowest_protocol_version`, `is_acceptable_version`).
- `tls_record_1_3.erl` — TLS 1.3 record encoding/decoding (inner plaintext, padding).
- `dtls_record.erl` — DTLS record layer (epochs, replay detection, `get_connection_state_by_epoch`).

### Cipher / version / group negotiation
- `ssl_cipher.erl` (58 KB) — cipher-suite maps, hashing/MAC helpers, suite selection.
- `ssl_cipher_format.erl` (72 KB) — conversions between the many cipher-suite representations (atoms, maps, OpenSSL names, RFC names).
- `tls_v1.erl` (53 KB) / `dtls_v1.erl` — per-protocol record ciphering, PRF and finished verification.
- `ssl_dh_groups.erl` — named DH/FFDHE groups (RFC 3526 primes, etc.).

### Sessions & tickets
- `ssl_session.erl` — the `#session{}` record and validation/upgrade logic.
- `ssl_session_cache_api.erl` — public behaviour for custom session-cache callbacks (since OTP R14B).
- `ssl_client_session_cache_db.erl` (client DB), `ssl_server_session_cache.erl` (gen_server), `ssl_server_session_cache_db.erl` (TLS 1.2 DB), `ssl_server_session_cache_sup.erl`, `ssl_upgrade_server_session_cache_sup.erl`, `dtls_server_session_cache_sup.erl`.
- `tls_server_session_ticket.erl` + `_sup` — TLS 1.3 stateless/stateful server session tickets.
- `tls_client_ticket_store.erl` — client-side TLS 1.3 ticket storage.
- `tls_bloom_filter.erl` — anti-replay bloom filter for TLS 1.3 stateless tickets.

### Certificates / PKIX / CRL
- `ssl_pkix_db.erl` — storage for trusted certificates (per connection, ETS-backed).
- `ssl_pem_cache.erl` — caches decoded PEM file contents.
- `ssl_certificate.erl` — certificate path validation (RFC 3280) helpers called from `ssl_handshake`.
- `ssl_manager.erl` — `gen_server` managing session storage and trusted-cert DB (registered name `ssl_manager`).
- `ssl_crl.erl`, `ssl_crl_cache.erl` (default cache), `ssl_crl_cache_api.erl` (public behaviour since OTP 18.0), `ssl_crl_hash_dir.erl` (hash-dir cache).

### Supervision & listeners
- `tls_sup.erl`, `tls_server_sup.erl`, `tls_connection_sup.erl`, `tls_dyn_connection_sup.erl` — `tls_dyn_connection_sup` supervises the `tls_sender` + `ssl_gen_statem` pair (`one_for_all`, `auto_shutdown => any_significant`).
- `dtls_sup.erl`, `dtls_server_sup.erl`, `dtls_connection_sup.erl`, `dtls_listener_sup.erl`, `dtls_server_session_cache_sup.erl`.
- `dtls_packet_demux.erl` — multiplexes one UDP socket across many DTLS connections (one process per `{IP,Port}` listener, registered in the `dtls_listener_sup` ETS table); handles "emulated" socket options per connection.
- `ssl_listen_tracker_sup.erl` — tracks listen sockets (also used by DTLS over SCTP).

### Sockets (transport abstraction)
- `tls_socket.erl` — `gen_server` that owns the transport socket and mediates emulated socket options for TLS.
- `tls_socket_tcp.erl` — alternative transport using `socket` (raw sockets) directly instead of `gen_tcp`; also the entry point for kTLS/`ktls` option support (`cb_info/0` returns `'$socket'` tags).
- `dtls_socket.erl` — DTLS socket abstraction built on `dtls_packet_demux`.

### Erlang distribution over TLS
- `inet_tls_dist.erl` (37 KB) — TLS distribution transport module (`dist` behaviour) incl. kTLS helpers (`set_ktls/1`, `inet_ktls_setopt/3`, `ktls_opt_cipher/6`).
- `inet6_tls_dist.erl` — thin IPv6 wrapper delegating to `inet_tls_dist:fam_*`.
- `inet_epmd_tls_socket.erl`, `ssl_dist_sup.erl`, `ssl_dist_admin_sup.erl`, `ssl_dist_connection_sup.erl`, `tls_dist_sup.erl`, `tls_dist_server_sup.erl`.

### Logging & tracing
- `ssl_logger.erl` — structured logging bridge to `logger` (keylog, `?SSL_LOG`).
- `ssl_trace.erl` — `dbg`-based tracing support (`behaviour` module used by `ssl:handle_trace/3`).

### Internal headers (all in this dir)
- `ssl_internal.hrl` — global macros: version lists (`ALL_SUPPORTED_VERSIONS`, `ALL_AVAILABLE_VERSIONS`), binary constructors, `?SSL_LOG`, `#config{}`, the `*_ONLY_OPTIONS` option-group macros.
- `ssl_api.hrl` — `#sslsocket{}` record (the opaque `ssl:sslsocket()`; `connection_cb` is `tls_gen_connection | dtls_gen_connection`).
- `ssl_connection.hrl` — the shared `#state{}` plus `#static_env{}`, `#handshake_env{}`, `#connection_env{}`, `#recv{}`.
- `tls_connection.hrl` / `dtls_connection.hrl` — protocol-specific `#protocol_buffers{}` (each includes `ssl_connection.hrl`).
- Record layer headers: `ssl_record.hrl` (`#security_parameters{}`), `tls_record.hrl`, `tls_record_1_3.hrl`, `dtls_record.hrl`.
- Handshake headers: `ssl_handshake.hrl` (shared records/constants), `tls_handshake.hrl` (`#client_hello{}` etc.), `tls_handshake_1_3.hrl`, `dtls_handshake.hrl`.
- `ssl_cipher.hrl`, `ssl_alert.hrl`, `ssl_srp.hrl`.

## Design Decisions
Layered architecture: public API (`ssl`) → generic state machine (`ssl_gen_statem` + `tls_dtls_*`/`tls_gen_connection`/`dtls_gen_connection`) → handshake layer (`ssl_handshake` + `tls_handshake`/`tls_handshake_1_3`/`dtls_handshake`) → record layer (`ssl_record` + `tls_record`/`tls_record_1_3`/`dtls_record`) → `crypto`/`public_key`.
Generic/common logic lives in the unprefixed `ssl_*` modules (`ssl_record`, `ssl_handshake`, `ssl_session`, `ssl_cipher`), with `tls_*` and `dtls_*` modules providing the protocol specialisation; this is achieved by *call*, not by a formal behaviour — the `ssl_*` modules call `tls_record:`/`dtls_record:` etc.
The connection `gen_statem` process is generic (`ssl_gen_statem`) and selects a version-specific module at init; the TLS and DTLS families are kept separate (`dtls_*` vs `tls_*`) rather than parameterised further.
TLS 1.3 uses distinct `*_1_3` modules throughout (connection, handshake, record) because its handshake, key schedule and record format differ enough from 1.2 that sharing would obscure both.
Sending is off-loaded to a dedicated `tls_sender` process so that record encryption and socket writes do not block the receiving state machine (also needed for TLS 1.3 post-handshake key updates).
`connection_cb` in `#sslsocket{}` (`tls_gen_connection` | `dtls_gen_connection`) is the discriminator `ssl` uses to route many API calls without inspecting version.
Headers are internal to `src/` (nothing exported at `include/`), so they include each other by bare filename (`-include("ssl_record.hrl")`) and the compiler finds them in the source dir.

## Constraints
- The app must only depend on `crypto`, `public_key`, `kernel`, `stdlib` (see `ssl.app.src`); never on higher-level OTP apps such as `inets`, `runtime_tools` (those appear only as optional runtime deps for tests).
- Any new module must be added to both `MODULES`/`BEHAVIOUR_MODULES` in `Makefile` and to the `modules` list in `ssl.app.src`.
- `src/ebin/*.app` and `src/ebin/*.appup` are generated from `ssl.app.src`/`ssl.appup.src` by `sed` substituting `%VSN%`; never edit the generated files or hardcode the version.
- `EXTRA_ERLC_FLAGS` in `Makefile` include `-Werror`, so any new warning (unused var, obsolete bool op) breaks the build.
- Included from `$(ERL_TOP)/lib/kernel/src`; do not add `include_lib` cycles back into kernel internals.

## Known Issues / Notes for Agents
- Sizing: `ssl.erl` (163 KB), `ssl_handshake.erl` (196 KB), `ssl_gen_statem.erl` (114 KB), `ssl_config.erl` (95 KB), `tls_handshake_1_3.erl` (99 KB) are enormous single files — grep by function/`-spec` rather than reading top-to-bottom.
- Several modules deliberately warn-suppress `erlang:list_to_atom/1` and `erlang:binary_to_term/2` via `-compile([{nowarn_possibly_unsafe_function, ...}])` (`ssl_config`, `inet_tls_dist`, `ssl_manager`, `ssl_pem_cache`, `ssl_session`, `tls_dtls_client_connection`, several `*_session_cache_sup`); do not "clean these up" without understanding the option-atom parsing they rely on.
- `ssl_record.erl`, `tls_record.erl`, `dtls_record.erl` and `ssl_cipher.erl` use `-compile(inline)`; `ssl.erl` inlines `encode_packet/2`.
- User-supplied session-cache and CRL-cache modules plug in via the `ssl_session_cache_api` / `ssl_crl_cache_api` behaviours; the built-in `*_db`/`ssl_crl_cache` modules are just default implementations.
- DTLS "emulated socket options" are managed per connection inside `dtls_packet_demux`, not on the raw UDP socket — option changes go through the demux process.
- `ssl_gen_statem:ssl_config/3` pre-1.3 options are grouped by the `?'TLS-1_3_ONLY_OPTIONS'`, `?'PRE_TLS-1_3_ONLY_OPTIONS'`, `?'FROM_TLS-1_2_ONLY_OPTIONS'`, `?'TLS-1_0_ONLY_OPTIONS'` macros in `ssl_internal.hrl`; option validity is version dependent.
- Nothing in this directory is code-generated (the `configure`/`Makefile` under `lib/ssl/` are the only generated artifacts of the app).
- `-DVSN=\"$(VSN)\"` is passed to the compiler; `ssl.erl`/app modules read it only for version reporting.

## Dependencies
Runtime: `crypto` (primitive ciphers/hashes/PK), `public_key` (cert/keys/ASN.1), `kernel` (sockets/`inet`/`gen_tcp`/`gen_udp`/`logger`/`code`), `stdlib`.
Build: GNU autoconf/make via `lib/ssl/Makefile` + `vsn.mk` (`SSL_VSN`); no external C libraries of its own (it links nothing, all crypto is via `crypto`).
Optional at runtime: kTLS requires OS/`socket` support (Linux/kTLS-capable kernels); DTLS-over-SCTP is driven through the `listen_tracker` infra.

## Test Strategy
Test suites live in `lib/ssl/test/` (Common Test, run with `make test` from `lib/ssl/`) — not in this directory.
Many suites are named `*_SUITE.erl` per feature (`tls_handshake_SUITE`, `ssl_cipher_SUITE`, `dtls_SUITE`, `tls_version_SUITE`, `ssl_session_ticket_SUITE`, …); unit-level helpers such as `ssl_cipher_format` and `ssl_config` are exercised indirectly through those.
This directory contains no tests; nothing here is generated so `git ls-files` is authoritative for what ships.

## See Also
- `../include/` — holds only `.gitignore`; no public headers are exported by `ssl`.
- `../test/` — Common Test suites for the app (read-only cross-reference).
- `../doc/` — user documentation (`ssl_app.md`, `guides/*.md`).
- `../internal_doc/pem_and_cert_cache.md` — design notes for `ssl_manager`/`ssl_pem_cache`.
- `../internal_doc/ssl_notes.md` — further internal design notes.
- `../../public_key/` and `../../crypto/` — the underlying certificate/crypto APIs this app builds on.
- `../../kernel/src/inet_tls_dist.erl`-adjacent infrastructure (`dist_util`, `net_kernel`) — the distribution side that drives `inet_tls_dist`.
