# megaco — Protocol Engine

## Intent
Core of the Megaco/H.248 protocol stack: connection/configuration management, the message messenger (transaction send/receive state machine) and the public behaviour contracts that users implement.
It sits between the codecs (`../text/`, `../binary/`, `../flex/`) and the transports (`../udp/`, `../tcp/`), and between those and the top-level API module `../app/megaco.erl`.
It is the only place where user callbacks, the messenger's timers/statistics and the per-node configuration tables live.

## API Surface
Contents: 21 Erlang modules (`modules.mk` splits 4 `BEHAVIOUR_MODULES` from 17 `MODULES`) plus the internal header `megaco_message_internal.hrl`.

### Public behaviour contracts (stable, user-facing — do not break callback signatures)
- `megaco_user.erl` — the main user callback behaviour; 11 callback pairs, each in an `/N` and an `/N+1` "Extra"-argument variant (`handle_connect/2,3`, `handle_disconnect/3`, `handle_syntax_error/3,4`, `handle_message_error/3,4`, `handle_trans_request/3,4`, `handle_trans_long_request/3,4`, `handle_trans_reply/4,5`, `handle_trans_ack/4,5`, `handle_unexpected_trans/3,4`, `handle_trans_request_abort/4,5`, `handle_segment_reply/5,6`). Exports the types `receive_handle/0`, `conn_handle/0`, `megaco_timer/0`.
- `megaco_encoder.erl` — message/transaction codec behaviour: `encode_message/3`, `decode_message/3`, `decode_mini_message/3`, `encode_transaction/3`, `encode_action_requests/3`, `encode_action_reply/3` (all take an `EncodingConfig` first). `encode_action_reply/3` is optional. Also defines the shared `protocol_version()`, `megaco_message()`, `transaction()`, `action_request()/action_reply()` types re-exported by `megaco`.
- `megaco_transport.erl` — transport send behaviour: `send_message/2` (mandatory), `send_message/3` and `resend_message/2` (optional). Returning `{cancel, Reason}` is a normal, non-error outcome.
- `megaco_edist_compress.erl` — erlang-distribution message compression behaviour: `encode/2`, `decode/2` over Megaco message/transaction/action records.

### Documented utility modules (public API, documented since OTP 27.0)
- `megaco_digit_map.erl` — parses and evaluates H.248 digit maps (`parse/1`, `eval/1,2`, `report/2`, `test/2`); exports the types re-exported by `megaco`.
- `megaco_sdp.erl` — RFC 4566 SDP encode/decode helper (`megaco_sdp:encode/1`, `decode/1`); exports the SDP types re-exported by `megaco`.

### Behaviour implementations
- `megaco_user_default.erl` — no-op/logging default `megaco_user` implementation used when the user supplies no callback module.
- `megaco_erl_dist_encoder.erl` — a `megaco_encoder` codec based on `erlang:term_to_binary/2`; honours `megaco_compressed` / `{megaco_compressed, Mod}` encoding-config options, delegating compression to `megaco_erl_dist_encoder_mc`.
- `megaco_erl_dist_encoder_mc.erl` — the `megaco_edist_compress` implementation (version-dependent record-to-compact-term mapping) used by `megaco_erl_dist_encoder`.

### Internal implementation modules
- `megaco_config.erl` — `gen_server` owning all per-node configuration; one instance per node, registered as `megaco_config`. Holds the ETS tables `megaco_config` (per-user key/value, keypos 1), `megaco_local_conn` (keypos 2, one `#conn_data{}` per local connection) and `megaco_remote_conn` (keypos 2, `bag`). Exposes `start_user/stop_user`, `user_info/2`, `update_user_info/3`, `conn_info/2`, `update_conn_info/3`, `system_info/1`, transaction/reply/pending counters, `connect/4`, `autoconnect/4`, `disconnect/1`, `connect_remote/3`, `disconnect_remote/2`, `lookup_local_conn/1`.
- `megaco_messenger.erl` — the largest module (≈5.4k lines); the heart of the engine. Implements `megaco:connect/disconnect/call/cast/cancel`, `receive_message/4,5`, `process_received_message/4,5,6`, `encode_actions/3`, the request/reply/pending/segment/keep-alive timeout handlers, `cleanup/2`, `which_requests/1`, `which_replies/1`, `test_request/5`, `test_reply/5` and the MIB statistics entry points. Tracks in-flight traffic in the `megaco_requests`/`megaco_replies` ETS tables owned by `megaco_monitor`; `#request{}`/`#reply{}`/`#trans_id{}` are defined here.
- `megaco_messenger_misc.erl` — encoding/segmentation and send helpers used by the messenger (`encode_body/3`, `encode_trans_request/2`, `encode_trans_reply/2`, `encode_actions/3`, `send_body/3`, `send_message/3`, `transform_transaction_reply/2`).
- `megaco_monitor.erl` — `gen_server` that owns the `megaco_requests` and `megaco_replies` ETS tables; provides lookups/pattern matching/field updates on them plus `apply_after/4,5` and `apply_at_exit/4` timer/exit-hook services used by the messenger.
- `megaco_trans_sender.erl` — per-connection batching/lotsizing process (one per `{megaco_trans_sender, ConnHandle}` under `megaco_trans_sup`) that accumulates transaction requests and acks and flushes them; it is the caller of the user `megaco_transport` module.
- `megaco_trans_sup.erl` — `supervisor` for the `megaco_trans_sender` processes (`start_trans_sender/5`); temporary children keyed by `{megaco_trans_sender, ConnHandle}`.
- `megaco_misc_sup.erl` — empty `one_for_one` supervisor used to host ad-hoc permanent workers via `start_permanent_worker/4`.
- `megaco_sup.erl` — the megaco application's top supervisor (`-behaviour(application)`; it is the `mod` in `megaco.app.src`). Starts `megaco_misc_sup`, `megaco_trans_sup`, `megaco_config` and `megaco_monitor`.
- `megaco_config_misc.erl` — pure validation helpers (`verify_bool/1`, `verify_int/1,2,3`, `verify_strict_int/…`, `verify_uint/…`) used by config option validation.
- `megaco_timer.erl` — arithmetic/validation for `#megaco_incr_timer{}` (`init/1`, `restart/1`, `verify/1`).
- `megaco_stats.erl` — generic named-ETS counter helper (`init/1,2`, `inc/2,3,4`, `get_stats/1,2,3`, `reset_stats/1,2`) reused by other megaco subdirectories.
- `megaco_filter.erl` — Megaco customisation of the Event Tracer (`et`) viewer/active filter (`start/0,1`, `filter/1`, `raw_filter/1`, `pretty_error/1`, `string_to_term/1`).
- `megaco_message_internal.hrl` — cross-subdirectory internal record header (see Constraints).

## Constraints
- The four behaviour modules are public API: callback names/arities and their types must stay backwards compatible within a major release.
- Every `megaco_user` callback has both a base and an `Extra`-argument variant; the `Extra` variant is only called when a message was received with an `Extra` other than `ignore_extra` (see `?default_user_callback_extra`).
- `megaco_message_internal.hrl` is an internal cross-subdirectory contract: `../text/`, `../binary/` and engine modules all `-include_lib("megaco/src/engine/megaco_message_internal.hrl")`; changing a record here breaks every codec subdirectory.
- `megaco_message_internal.hrl` is checked in but carries a "Generated by the Erlang ASN.1 compiler" banner and hand-maintained deviations; it must not be regenerated blindly as it deliberately differs from `include/megaco_message_v1..v3.hrl`.
- There is exactly one `megaco_config` and one `megaco_monitor` per node (both registered names in `megaco.app.src`); all engine state below them is per-user (keyed by local Mid) and per-connection (keyed by `#megaco_conn_handle{}`).
- All engine state mutation of shared tables goes through `megaco_config`/`megaco_monitor` calls so updates are serialised; the `gen_server` state records only carry `parent_pid`.
- New modules must be added to the correct list in `modules.mk` (`BEHAVIOUR_MODULES` vs `MODULES`) and, if they `-include` something, to `depend.mk`.
- Do not document/edit `../../ebin/` build output; it is not tracked.

## Design Decisions
- Configuration and request/reply bookkeeping are kept in `public, named_table` ETS tables owned by long-lived `gen_server`s (`megaco_config`, `megaco_monitor`) rather than in process state, so hot paths read directly from ETS and only writes are serialised through the servers.
- Incoming messages are handled by a freshly spawned process per message (`spawn_opt` with `link` and `{min_heap_size, 5000}`); `megaco_messenger:receive_message/4,5` returns immediately after spawning. With `threaded = true` each transaction request is additionally handled in its own spawned process.
- Transaction sending is decoupled from request handling: `megaco_trans_sender` batches outgoing requests/acks on a timer and calls the user `megaco_transport` module, rather than sending directly from the messenger.
- `megaco_trans_sender` is started with `proc_lib:start_link/3` and implements the `sys` protocol itself (`system_continue/3`, `system_terminate/4`, `system_code_change/4`) despite being listed with `gen_server` in its supervisor child spec.
- `megaco_digit_map` and `megaco_sdp` were promoted from internal helpers to documented public modules in OTP 27.0; their types are re-exported from `megaco`.

## Known Issues
- `#megaco_receive_handle{}` and `#megaco_conn_handle{}` are defined in `include/megaco.hrl`, not here — but `megaco_user.erl` re-exports them as `receive_handle()`/`conn_handle()`, so the type's owning header and its documented name live in different files.
- `megaco_filter.erl` cannot `-include` `megaco_message_internal.hrl` because that file duplicates records from `megaco_message_v1..v3.hrl` and `megaco_udp.hrl`/`megaco_tcp.hrl` (conflicting `HEAP_SIZE`/`GC_MSG_LIMIT` macros); it therefore keeps local copies of `'megaco_transaction_reply'`, `megaco_udp` and `megaco_tcp`. Those copies silently drift from the originals.
- `megaco_filter.erl` only handles protocol version 1 (noted in its own source).
- `megaco_digit_map` depends on `../text/megaco_text_tokens.hrl`, an engine→text include direction that is unusual for this codebase (engine is otherwise the lower layer).
- `megaco_messenger:cancel/2` relies on matching whole `#request{}`/`#reply{}` records; the module carries explicit "N.B. Update cancel/1 with '_' when a new field is added" reminders, so adding a field without updating those clauses breaks cancellation.
- `megaco_config` retains a number of "for backward compatibility (code upgrade)" call clauses and deprecated argument forms.

## Notes for Agents
- `megaco_messenger.erl` (5376 lines) and `megaco_config.erl` (2189 lines) are by far the largest files; never read them whole — grep for exports/section banners (`%%---`) and read targeted ranges.
- `megaco_erl_dist_encoder_mc.erl` (1898 lines) is generated-style repetitive per-record/per-version compression code; treat it as machine-like and edit only the relevant record clauses.
- Section banners in the large modules encode the structure: "Register/unregister connections", "Handle incoming message", "SNMP statistics handling functions", "cleanup utility functions" etc.
- Several modules carry `MEGACO_TEST_CODE` / `debug` compile-time branches (see `../app/megaco.mk`) — test-only code paths are `-ifdef`-guarded and not compiled in a normal build.
- `megaco_stats.erl` is a generic ETS counter helper, not megaco-protocol-specific; it is used from `../binary/`, `../text/`, `../udp/`, `../tcp/` as well.

## Dependencies
- `-include_lib("megaco/include/megaco.hrl")` and `-include_lib("megaco/src/app/megaco_internal.hrl")` are the normal headers; `-include_lib("megaco/include/megaco_message_v1.hrl")` is used by `megaco_filter`, `megaco_user`, `megaco_user_default` and `megaco_sdp`.
- Compile flags come from `../app/megaco.mk` and add `-I../../include` plus `-pa $(ERL_TOP)/lib/et/ebin -pa $(ERL_TOP)/lib/megaco/ebin` (the `et` path is needed by `megaco_filter`).
- `megaco_filter` requires the `et` application; `megaco_user_default`/`megaco_monitor` logging goes through `logger` macros in `megaco_internal.hrl`.
- Build: `Makefile` compiles all modules into `../../ebin`, adds `-Ddebug` for `TYPE=debug`, and installs sources plus `megaco_message_internal.hrl` into `$(RELSYSDIR)/src/engine`.

## Test Strategy
- Engine suites live in `../test/` (other directory); this directory has no tests of its own.
- The messenger's behaviour is exercised end-to-end through the codec and transport suites, since `megaco:call/cast/receive_message` are covered there.
- Test-only hooks are compiled in with `USE_MEGACO_TEST_CODE=true` (defines `MEGACO_TEST_CODE`), which enables `ets:new(megaco_test_data, …)` and the `?SIM`/`?TC_AWAIT_SEND_EVENT` macrnos in `megaco_messenger_misc`.

## See Also
- `../app/` — top-level `megaco` API module, `megaco.app.src`/`megaco.appup.src`, `megaco_internal.hrl` (logger macros, `?default_user_callback_extra`), build flags in `megaco.mk`.
- `../text/`, `../binary/`, `../flex/` — text and binary/ASN.1 codecs; all include `engine/megaco_message_internal.hrl`.
- `../udp/`, `../tcp/` — transport implementations that call `megaco_messenger:receive_message/…` and implement `megaco_transport`.
- `../include/megaco.hrl` — `#megaco_receive_handle{}`, `#megaco_conn_handle{}`, `#megaco_incr_timer{}`.
