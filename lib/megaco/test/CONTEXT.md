# Megaco Test Suites

## Intent
Common Test suites plus shared test infrastructure for the Megaco/H.248 application (`../src/`).
Covers the codec (text/binary/erl_dist, protocol v1/v2/v3), transports (tcp/udp/generic), transaction/pending handling, config, MIB, digit-map, SDP, segmentation and load/benchmark behaviour.
Flat directory — no subdirectories.

## API Surface
Common Test suites (`*_SUITE.erl`, 23 total; every suite declares `suite() -> [{ct_hooks,[ts_install_cth]}].`):
- Codec: `megaco_codec_v1_SUITE`, `_v2_`, `_v3_` — per-protocol-version text/binary/erl_dist/pretty encode/decode conformance (files are huge; message tables live in `megaco_test_msg_v{1,2,3}_lib`).
- Codec: `megaco_codec_mini_SUITE` (mini text codec), `megaco_flex_SUITE` (flex scanner).
- Transports: `megaco_tcp_SUITE`, `megaco_udp_SUITE`, `megaco_trans_SUITE` (transaction sender/acks).
- Requests/transactions: `megaco_mess_SUITE` (ITU-T H.248 implementation), `megaco_mreq_SUITE` (req/reply, pending, cancel), `megaco_pending_limit_SUITE`, `megaco_actions_SUITE` (independent encode + request-action handling).
- Other: `megaco_binary_term_id_SUITE` (encode-first/decode-first term ids), `megaco_call_flow_SUITE` (sample call flows, distributed nodes), `megaco_config_SUITE` (app env, transaction-id counter), `megaco_digit_map_SUITE`, `megaco_sdp_SUITE`, `megaco_segment_SUITE` (v3 segmentation), `megaco_mib_SUITE`, `megaco_examples_SUITE` (simple + meas/bench), `megaco_timer_SUITE`, `megaco_app_SUITE` (app/appup), `megaco_load_SUITE` (load + bench).

Shared infrastructure helper modules (not suites):
- `megaco_test_lib` (+ `megaco_test_lib.hrl`) — the central test library and lightweight test server. Provides the macros suites use (logging/verify/skip), `init_per_suite`/`end_per_suite`, node control, inet-backend selection, socket `open/listen/connect` and tracing.
- `megaco_test_mg` / `megaco_test_mgc` — in-test MG and MGC entities implementing the megaco user callback API (`start`/`stop`, stats, user/conn info, request-handling modes).
- `megaco_test_generator` (a gen_server behaviour) + `megaco_test_generator_lib`, `megaco_test_megaco_generator`, `megaco_test_tcp_generator` — sequence generators for megaco/tcp traffic.
- `megaco_test_msg_v{1,2,3}_lib` — message/type builders per protocol version (generated-style tables).
- `megaco_codec_test_lib`, `megaco_codec_flex_lib` — codec test helpers.
- `megaco_test_deliver` (stands in for the `megaco` module), `megaco_test_command_handler`, `megaco_test_generic_transport` (simulated transport), `megaco_tc_controller` (test-case key/value table), `megaco_test_sys_monitor` / `megaco_test_global_sys_monitor` (system-event monitors), `megaco_profile` (profiling driver), `megaco_appup_mg` / `megaco_appup_mgc` (appup test entities), `megaco_mess_user_test` / `megaco_mess_otp8212_test` (sample user callbacks).

How a suite typically wires up an MG/MGC pair:
- `init_per_suite` calls `megaco_test_lib:init_per_suite/1`, then starts `megaco_test_sys_monitor` (and `megaco_test_global_sys_monitor` per testcase).
- Local entities: `megaco_test_mgc:start/4` (`Pid, Mid, EncoderTypes, Verbosity`) and `megaco_test_mg:start/5` (`Pid, Mid, {Codec,EncoderConfig}, Transport, Verbosity`); used by `actions`, `load`, `mess`, `mreq`, `pending_limit`, `trans`.
- Distributed cases acquire/start extra nodes via the `?ACQUIRE_NODES`/`?MK_NODES`/`?START_NODES` macros (`megaco_test_lib:prepare_test_case`, `mk_nodes`), then start the MG/MGC remotely through `megaco_test_lib:proxy_start/2` (used by `call_flow`, `codec_v*`, `config`, `mess`, `udp`).
- `examples` starts the shipped `megaco_simple_mg`/`megaco_simple_mgc` examples directly.

## Test Strategy / Notes for Agents
- Run from `lib/megaco/`: `make test` (Common Test). The app `Makefile` sets `TEST_NEEDS_RELEASE=true`, so an installed OTP release must be present.
- The test release dir is `megaco_test` (`RELSYSDIR = $(RELEASE_PATH)/megaco_test`); `make release_tests_spec` installs sources, specs and `../examples` there.
- `megaco.spec` selects all suites (`{suites,"../megaco_test",all}`); `megaco.cover` uses `{incl_app,megaco,details}` and excludes generated encoder/parser and message modules.
- `megaco_bench.spec` runs only the `bench` groups of `megaco_examples_SUITE` and `megaco_load_SUITE` — most bench cases are explicitly gated (not in `all/0`).
- Dialyzer PLT is built with `DIA_PLT_APPS=asn1 runtime_tools et debugger` (see `../Makefile`).
- Many suites gate extra runs behind `inet_backend` groups (`inet_backend_default`/`_inet`/`_socket`); the `inet`/`socket` groups are skipped unless `?TEST_INET_BACKENDS()`/env allows them.
- Slow / load suites: `megaco_load_SUITE` (standard/single/multi + bench/bench_single/bench_multi), `megaco_examples_SUITE` (meas/bench), `megaco_mess_SUITE`, `megaco_codec_v{1,2,3}_SUITE`, `megaco_segment_SUITE`, `megaco_call_flow_SUITE`. Entry files are enormous; expect `?SLEEP` delays in transport/mess cases.
- `Makefile` extras: `tests`/`debug`/`opt` (compile), `clean`, and convenience targets (decode/encode profiling, `time*`/`tt*`, `display_*`/`generate_*` message dumps, `gnuplot_gif`, `node`).
- The `utest`/`ftest` make targets invoke `megaco_test_lib t <SUITE>`; there is no `t/1` in `megaco_test_lib` — use the Common Test path instead.

## Constraints
- Every compiled module (suite or helper) MUST be listed in `modules.mk` (`TEST_UTIL_MODULES` / `SUITE_MODULES`); `Makefile` derives `ERL_FILES`/`TARGET_FILES` from `MODULES`.
- Shared header is `megaco_test_lib.hrl` (the only `HRL_FILES` entry); helpers `-include` it.
- Suites and helpers rely on `../../megaco/examples/simple`, `../../megaco/ebin` and `../../et/ebin` being on the code path (`ERL_PATH` in `Makefile`).
- `*.log` and `*.beam` under `test/` are git-ignored build/test output.

## Routing Table
- Codec conformance v1/v2/v3 → `megaco_codec_v1_SUITE`, `megaco_codec_v2_SUITE`, `megaco_codec_v3_SUITE` (+ `megaco_test_msg_v{1,2,3}_lib`, `megaco_codec_test_lib`)
- Mini/flex codec → `megaco_codec_mini_SUITE`, `megaco_flex_SUITE` (+ `megaco_codec_flex_lib`)
- Transports → `megaco_tcp_SUITE`, `megaco_udp_SUITE`, `megaco_trans_SUITE` (+ `megaco_test_generic_transport`, `megaco_test_tcp_generator`)
- Requests/transactions/pending → `megaco_mess_SUITE`, `megaco_mreq_SUITE`, `megaco_pending_limit_SUITE`, `megaco_actions_SUITE`
- App/config/mib/timer/appup → `megaco_app_SUITE`, `megaco_config_SUITE`, `megaco_mib_SUITE`, `megaco_timer_SUITE` (+ `megaco_appup_mg`, `megaco_appup_mgc`)
- Digit-map/SDP/segment → `megaco_digit_map_SUITE`, `megaco_sdp_SUITE`, `megaco_segment_SUITE`
- Call-flow/examples → `megaco_call_flow_SUITE`, `megaco_examples_SUITE`
- Load/benchmark → `megaco_load_SUITE`, `megaco_examples_SUITE` (meas/bench groups), `megaco_profile`, `megaco_bench.spec`
- Shared test infrastructure → `megaco_test_lib` (+`.hrl`), `megaco_test_mg`, `megaco_test_mgc`, `megaco_test_generator` (+`_lib`/`megaco_test_megaco_generator`/`megaco_test_tcp_generator`), `megaco_test_deliver`, `megaco_test_command_handler`, `megaco_tc_controller`, `megaco_test_sys_monitor`, `megaco_test_global_sys_monitor`, `megaco_test_generic_transport`, `megaco_mess_user_test`, `megaco_mess_otp8212_test`

## See Also
- `../src/` — the megaco application sources under test.
- `../Makefile` — sets `TEST_NEEDS_RELEASE=true` and `DIA_PLT_APPS`.
- `../../et/` — the Event Tracer used by the `et_viewer` test driver and PLT.
