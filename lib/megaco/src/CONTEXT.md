# megaco/src — Protocol Stack Sources

## Intent
All sources of the `megaco` OTP application (Megaco/H.248, RFC 3525), split one subdirectory per architectural layer.
Layering, top to bottom: public API (`app/megaco.erl`) → protocol engine (`engine/`) → codecs (`text/` + `flex/`, `binary/`) and transports (`tcp/`, `udp/`) → user-supplied callback modules (`megaco_user`, `megaco_transport`, `megaco_encoder`).
`engine/megaco_messenger` is the hub: it decodes incoming binaries via the codec selected by the receive handle's `encoding_mod`/`encoding_config`, dispatches to the user's `megaco_user` module, and sends via the `send_mod` transport module.
There is no per-subdirectory `ebin`: every subdir compiles into the single application-wide `../ebin` (`EBIN = ../../ebin`), so the split is about include paths, generated code and build order, not about separately loadable units.
User-facing documentation is generated from the `-moduledoc`/`-doc` blocks of the public modules plus `../doc/`.

## API Surface
Public modules (documented, stable):
- `app/megaco.erl` — the whole public Erlang API (start/stop, start_user/2, connect/4,5, call/3, cast/3, receive_message/4,5, process_received_message/4,5, encode_actions/3, digit maps, stats, tracing, version info, test helpers).
- `engine/megaco_user.erl` — user callback **behaviour** (11 callback pairs, each with an `/N+1` "Extra" variant; all `optional_callback`).
- `engine/megaco_encoder.erl` — codec **behaviour** (`encode_message`, `decode_message`, `decode_mini_message`, `encode_transaction`, `encode_action_requests`, `encode_action_reply`) plus the shared `protocol_version()`/`megaco_message()`/transaction types.
- `engine/megaco_transport.erl` — transport **behaviour** (`send_message/2,3`, `resend_message/2`; returning `{cancel, Reason}` is a normal outcome).
- `engine/megaco_edist_compress.erl` — erlang-distribution compression **behaviour** (`encode/2`, `decode/2`).
- `engine/megaco_digit_map.erl`, `engine/megaco_sdp.erl` — documented utility modules (since OTP 27.0); their types are re-exported from `megaco`.
- `udp/megaco_udp.erl`, `tcp/megaco_tcp.erl` — named by the user in `connect/4,5`; they implement (partially — see Known Issues) the `megaco_transport` behaviour and expose a private transport-control API (`start_transport/0`, `stop_transport/1`, `open|listen`, `send_message/2`, `block/1`, `unblock/1`, `close/1`, `upgrade_receive_handle/2`, `get_stats`, `reset_stats`).
Behaviour implementations and internal-codec entry points the user selects by name: `engine/megaco_user_default` (default `user_mod`), `engine/megaco_erl_dist_encoder{,_mc}`, `text/megaco_pretty_text_encoder` (default `encoding_mod`), `text/megaco_compact_text_encoder`, `binary/megaco_binary_encoder` (BER), `binary/megaco_ber_encoder`, `binary/megaco_per_encoder`.
Everything else in `engine/`, `text/` (the `_v1/_v2/_v3` modules), `binary/` (resolvers, transformers, term-id handling), `flex/`, `tcp/`, `udp/` is internal implementation.
Public headers are NOT here — they live in the sibling `../include/` (`megaco.hrl`, `megaco_message_v{1,2,3}.hrl`, `megaco_sdp.hrl`), promoted to `$(RELEASE_PATH)/lib/megaco-$(VSN)/include` by `app/Makefile`.

## Build Layout
`Makefile` includes `$(ERL_TOP)/make/target.mk`, `make/$(TARGET)/otp.mk`, `subdirs.mk` and `$(ERL_TOP)/make/otp_subdir.mk`, and adds `SPECIAL_TARGETS = setup` with `setup: (cd flex && $(MAKE) $@)`.
`subdirs.mk` fixes `SUB_DIRECTORIES = app engine text flex binary tcp udp`; `app` must be first so `megaco.beam` is on the code path used to compile the rest.
Every subdir `Makefile` sets `EBIN = ../../ebin` and `MEGACO_INCLUDEDIR = ../../include`, includes `../../vsn.mk` (`VSN = $(MEGACO_VSN)`), its own `modules.mk`, the shared compile-flag file `../app/megaco.mk` (except `app/` and `flex/`), its own `depend.mk` (except `flex/`, `udp/`) and `$(ERL_TOP)/make/otp_release_targets.mk`.
`flex/` is special: its tracked `Makefile` is a thin wrapper over `$(ERL_TOP)/make/run_make.mk` that recurses into `$(TARGET)/Makefile`, which the app's `configure` generates from `Makefile.in`.

## Constraints
Module lists are per-subdirectory (`<subdir>/modules.mk`); a new module must be added there and, if it has includes, to `depend.mk`.
`app/megaco.app.src` carries a hand-maintained 65-entry `modules` list (55 tracked modules + 10 build-generated ones) that must be updated by hand for every module added anywhere under `src/`; it is not derived from the build.
Generated artifacts must never be hand-edited (all gitignored): Yecc output `text/megaco_text_parser_v{1,2,3}.erl` and `text/megaco_text_mini_parser.erl` (`.yrl` → `.erl` rule in `$(ERL_TOP)/make/otp.mk`, generated from `make/otp.mk.in`); ASN.1 output `binary/megaco_{ber,per}_media_gateway_control_v{1,2,3}.erl|.hrl|.asn1db`; flex output `flex/megaco_flex_scanner_drv{,_mt}.flex|.c` and `flex/$(TARGET)/Makefile`.
The v1/v2/v3 (and pretty/compact, BER/PER) triplication is deliberate — do not factor the version-specific modules into one parameterized module.
`app/megaco_internal.hrl` and `engine/megaco_message_internal.hrl` are internal headers shared across subdirectories (19 modules include the former, `text/` and `binary/` include the latter); changing a record in either is an app-wide change.
Subdirectory boundaries are not code boundaries: `engine/megaco_digit_map.erl` includes `../text/megaco_text_tokens.hrl` (engine→text), and `text/`+`binary/` include `../engine/megaco_message_internal.hrl` (text/binary→engine).
Behaviour callback names/arities and their types are public API and must stay backward compatible within a major release.

## Design Decisions
One application-wide `ebin` per app rather than per-subdir ebins — subdirs exist to separate generated-code pipelines and include scopes.
Transports are deliberately NOT in the `megaco` supervision tree (`megaco.app.src` `registered` lists only `megaco_config`, `megaco_monitor`, `megaco_trans_sup`, `megaco_misc_sup`, `megaco_sup`): a user calls `start_transport/0` per transport instance.
Codecs and transports are hot-swappable through the receive handle: the user picks `encoding_mod`/`encoding_config` (codec, e.g. pretty/compact text, BER, PER, erlang-dist) and `send_mod` (transport) rather than a compile-time choice.
Codec modules are wired to the ASN.1-generated modules only at run time (module names passed as atoms), so only make-level ordering (in `depend.mk`) links them.
The `--enable-megaco-reentrant-flex-scanner` / `--enable-megaco-flex-scanner-lineno` configure options (driven by the app Makefile's `dconf`/`econf` targets) affect only `flex/`; nothing in `text/` is conditionally compiled on them.

## Known Issues
`rules.mk` is dead code: no Makefile anywhere in the repository includes it, and its `.yrl`/`.c.so`/`install` macros duplicate (stale versions of) what `$(ERL_TOP)/make/otp.mk` now provides.
`flex/` and `udp/` have no `depend.mk` (only `app/`, `engine/`, `text/`, `binary/`, `tcp/` do); their header dependencies are hand-written in their Makefiles, and `udp/Makefile` still has a stale rule for the removed `megaco_udp_test.erl`.
`engine/megaco_filter.erl` duplicates the `#megaco_tcp{}`/`#megaco_udp{}` records inline because `megaco_tcp.hrl` and `megaco_udp.hrl` both define `HEAP_SIZE`/`GC_MSG_LIMIT` and cannot be included together; those copies silently drift.
Neither transport implements the full `megaco_transport` behaviour: both export only `send_message/2`, so the `resend_indication` config option makes `send_message/3`/`resend_message/2` calls fail.
`binary/megaco_binary_encoder` is BER-only despite its generic name; PER is reachable only via `binary/megaco_per_encoder`.
All three binary codecs return `{error, not_implemented}` for `encode_transaction`/`encode_action_*`, so `megaco:encode_actions/3` and segmentation do not work with the binary codecs.
`tcp/megaco_tcp.hrl` / `udp/megaco_udp.hrl` cannot be included alongside each other, and `udp/Makefile`'s option tag (`udp_options`) differs from the documented `options` (same mismatch in `tcp/`).

## Notes for Agents
A fresh checkout has none of the generated codec/parser/scanner sources — build `lib/megaco` before grepping or reading them.
`app/megaco.erl` (~3200 lines), `engine/megaco_messenger.erl` (~5400 lines), `engine/megaco_config.erl` (~2200 lines) and `text/megaco_text_gen_v*.hrl` are enormous; navigate via `-export`/`-doc`/`-spec` and section banners, never read whole.
Compile-time feature knobs live in `app/megaco.mk` (`MEGACO_TRACE`, `MEGACO_EXTENDED_TRACE`, `MEGACO_DEBUG`, `MEGACO_PARSER_INLINE`, `USE_MEGACO_TEST_CODE`, `WARN_UNUSED_WARS`, `ERL_DETERMINISTIC`) — test-only code paths are `-ifdef`-guarded and absent from a normal build.
`flex/` needs an explicit `make setup` (creating `priv/lib/*.so` symlinks) before the C scanner is usable from a source tree; `SPECIAL_TARGETS` is not consumed by `make/`.
The app's own `configure`/`configure.ac` (sibling `../configure.ac`) only exists to configure the flex scanner.

## Dependencies
Build: `asn1` (its `erlc -bber`/`-bper` back ends generate `binary/`), plus for the optional text scanner real GNU `flex` (not `lex`) and `perl` (configure fails without it) and a C compiler with `erts/` headers.
Runtime: `applications = [stdlib, kernel]` only, but `app/megaco.app.src` also declares `runtime_dependencies` on `runtime_tools`/`dbg` and `et` (tracing), `debugger` (`megaco:ni/0`), and `asn1` (generated codec modules).

## Test Strategy
No test suites live under `src/`; the app's Common Test suites are in the sibling `../test/` (`megaco_codec_v{1,2,3}_SUITE`, `megaco_codec_mini_SUITE`, `megaco_binary_term_id_SUITE`, `megaco_flex_SUITE`, `megaco_tcp_SUITE`, `megaco_udp_SUITE`, `megaco_config_SUITE`, MG/MGC end-to-end helpers), driven from `lib/megaco/Makefile` via `make test`.
Codec changes must be validated per protocol version; the flex-vs-Erlang scanner comparison cases only run when the scanner was actually built.
Throughput/profiling benchmarks live in the sibling `../examples/meas/`, raw-API usage examples in `../examples/simple/`.

## Routing Table
| Path | Description |
| --- | --- |
| `app/` | Public API module `megaco.erl`, `megaco.app.src`/`.appup.src`, internal shared header `megaco_internal.hrl`, per-dir compile flags `megaco.mk` |
| `engine/` | Core engine: messenger, config (ETS state), monitor, supervision tree, stats/timer/filter helpers, digit map, SDP, the four public behaviour modules and their default implementations |
| `text/` | H.248 Annex B text codec: pretty + compact encoders, their `_v1/_v2/_v3` variants, Erlang scanner, mini decoder, Yecc grammars and token/value tables |
| `binary/` | H.248 Annex A ASN.1 BER and PER codecs: `.asn`/`.set.asn`/`.asn1config` inputs, encoders, name resolvers, record transformers, termination-id codec |
| `flex/` | Optional flex(1)-generated C linked-in driver that replaces the Erlang text scanner's tokenizing phase (configure/`make setup` dependent) |
| `tcp/` | TPKT over TCP/IP transport: listener, per-connection process, send/block/close primitives (started explicitly by the user) |
| `udp/` | UDP/IP transport: server process per socket, explicit per-peer send handles (started explicitly by the user) |
| `Makefile`, `subdirs.mk` | Subdir build driver and the fixed `SUB_DIRECTORIES` order (`app engine text flex binary tcp udp`) |
| `rules.mk` | Unreferenced legacy build rules — do not use as a rule source (see Known Issues) |
| `../include/` | Public exported headers (`megaco.hrl`, `megaco_message_v{1,2,3}.hrl`, `megaco_sdp.hrl`) (sibling — read-only, escalate writes to `lib/megaco`) |
| `../test/` | Common Test suites for the whole app (sibling — read-only, escalate writes to `lib/megaco`) |
| `../doc/`, `../examples/`, `../priv/` | ExDoc sources/guides, simple+meas examples, `priv/lib` scanner shared objects + `priv/plt` (siblings — read-only) |
| `../configure`, `../configure.ac`, `../vsn.mk`, `../Makefile` | App-level configure (flex scanner options), `MEGACO_VSN`, app build/subrelease driver (siblings — read-only) |
