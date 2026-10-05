# megaco — Megaco/H.248 Protocol Stack

## Intent
OTP application implementing Megaco/H.248 (RFC 3525) media-gateway control: codecs for the H.248 Annex A (ASN.1 BER/PER) and Annex B (text) encodings, a transport-agnostic protocol engine, and a public Erlang API for acting as a Media Gateway (MG) or Media Gateway Controller (MGC).
Built in place from `lib/megaco/` like every other `lib/<app>/` application.

## API Surface
- `src/app/megaco.erl` — the entire public Erlang API: `start/0`/`stop/0`, `start_user/2`/`stop_user/1`, `connect/4,5`/`disconnect/2`, `call/3`, `cast/3`, `cancel/2`, `receive_message/4,5`, `process_received_message/4,5`, `encode_actions/3`, digit-map functions (`parse_digit_map`, `eval_digit_map`, `report_digit_event`, `test_digit_event`), `encode_sdp/1`/`decode_sdp/1`, `encode_binary_term_id/2`/`decode_binary_term_id/2`, plus info, tracing (`enable_trace`/`disable_trace`/`set_trace`), stats and version helpers.
- There is no top-level `megaco:encode/N`/`megaco:decode/N`: decoding of incoming messages and encoding of `call/cast` requests happen inside the engine, and the codec is chosen per connection through the receive handle's `encoding_mod`/`encoding_config`.
- Behaviours (the user-supplied module interfaces): `src/engine/megaco_user.erl` (MG/MGC callbacks), `src/engine/megaco_encoder.erl` (codec: `encode_message`, `decode_message`, `decode_mini_message`, `encode_transaction`, `encode_action_requests`, `encode_action_reply`), `src/engine/megaco_transport.erl` (`send_message/2,3`, `resend_message/2`).
- Config/utility modules: `src/engine/megaco_config.erl` (registered ETS-backed config store), `src/engine/megaco_digit_map.erl`, `src/engine/megaco_sdp.erl`.
- Public record API: the headers in `include/` (`megaco.hrl`, `megaco_message_v{1,2,3}.hrl`, `megaco_sdp.hrl`), installed to `$(RELEASE_PATH)/lib/megaco-$(VSN)/include`.
- Build/release surface: `Makefile` (includes `vsn.mk`, `subdirs.mk`, `make/app_targets.mk`), `vsn.mk` (`MEGACO_VSN = 4.9.2`), `subdirs.mk` (`SUB_DIRS = src examples/simple examples/meas`), `configure` + `configure.ac`, `prebuild.skip`.

## Constraints
- `src/` is not flat: it builds per architectural layer, driven by `src/subdirs.mk` (`SUB_DIRECTORIES = app engine text flex binary tcp udp`), each layer having its own `modules.mk` (and usually `depend.mk`).
- A new module must be added to the correct `src/<layer>/modules.mk`, to the hand-maintained `modules` list in `src/app/megaco.app.src`, and (if it has includes) to the layer's `depend.mk`.
- All `src/` subdirs compile into the single app-wide `ebin/`; subdirs exist to separate generated-code pipelines and include scopes, not to form separate code paths.
- Generated code is gitignored and must never be hand-edited: Yecc `.erl` from `.yrl` (`src/text/`), ASN.1 BER/PER modules from `src/binary/*.asn`, the flex C driver (`src/flex/`).
- `configure` is autoconf output generated from `configure.ac`; edit `configure.ac` and regenerate with the repository-level `./otp_build update_configure`.
- `test/` requires an installed release (`TEST_NEEDS_RELEASE=true` in the app `Makefile`).
- `ebin/`, `priv/lib/*.so`, `priv/plt/*`, `doc/html` and the configured `examples/meas` scripts are build outputs.

## Dependencies
- `src/app/megaco.app.src`: `{applications, [stdlib, kernel]}`, `{mod, {megaco_sup, []}}`, `registered = [megaco_config, megaco_monitor, megaco_trans_sup, megaco_misc_sup, megaco_sup]`.
- Declared `runtime_dependencies` additionally name `runtime_tools`, `et` and `debugger` (tracing / `megaco:ni/0`) and `asn1` (generated codec modules); `compiler` is used by `megaco:nc/0` but is not declared.
- Build-time: `asn1` (`erlc -bber`/`-bper` for `src/binary/`), GNU `flex` (not `lex`) and `perl` for the optional flex scanner, and a C compiler.
- Dialyzer uses `DIA_PLT_APPS = asn1 runtime_tools et debugger` (set in the app `Makefile`).

## Notes for Agents
- `vsn.mk` defines `MEGACO_VSN` (currently `4.9.2`); the release-wide OTP→app-version mapping is the repo-root `otp_versions.table`.
- The app's own autoconf exists only to detect `flex` and select scanner options (`--enable-megaco-flex-scanner-lineno`, `--enable-megaco-reentrant-flex-scanner`) and to generate `examples/meas/Makefile` and `src/flex/$(host)/Makefile`.
- `prebuild.skip` lists the two flex-scanner beam files that bootstrap builds skip.
- The app `Makefile`'s `configure: configure.in` and `reconf` targets are stale leftovers (no `configure.in` exists; `reconf` cd's to a non-existent `$(ERL_TOP)/../libraries/megaco`) — use `./configure` or the top-level build instead.
- Codec, transport and user modules are all selected at run time per connection (`connect/4,5`), so switching encoding or transport needs no recompilation.
- Protocol versions v1/v2/v3 are deliberately triplicated (separate records, parsers, encoders and ASN.1 modules per version) — do not refactor into one parameterized implementation.

## Test Strategy
- 23 Common Test suites live in `test/`; run them with `make test` from `lib/megaco/` (needs an installed release).
- `make release_tests_spec` installs sources/examples into `$(RELEASE_PATH)/megaco_test`; `test/megaco.spec` selects all suites, `test/megaco.cover` is the coverage spec, `test/megaco_bench.spec` runs only `bench` groups.
- Codec correctness is covered per protocol version by `megaco_codec_v{1,2,3}_SUITE`; transports by `megaco_tcp_SUITE`/`megaco_udp_SUITE`/`megaco_trans_SUITE`.
- Slow suites: `megaco_load_SUITE`, the `bench` groups, `megaco_mess_SUITE` and `megaco_codec_v{1,2,3}_SUITE`.
- Throughput measurement tooling (not Common Test) lives in `examples/meas/` and is exercised by `megaco_examples_SUITE`.

## Routing Table
- `src/` → protocol stack sources: public API (`src/app/`), engine/behaviours (`src/engine/`), codecs (`src/text/`, `src/binary/`, `src/flex/`) and transports (`src/tcp/`, `src/udp/`) — see `src/CONTEXT.md` for the layer map
- `include/` → the 5 exported headers: internal handles/wildcards/error codes (`megaco.hrl`), the ASN.1-generated v1/v2/v3 message records, and the SDP records (`megaco_sdp.hrl`)
- `test/` → the 23 Common Test suites plus shared MG/MGC test infrastructure — see `test/CONTEXT.md`
- `doc/` → ExDoc config (`docs.exs`) and guides, `notes.md` release notes, verbatim standards in `standard/` — see `doc/CONTEXT.md`
- `examples/` → runnable samples (`examples/simple/`) and codec measurement/benchmark tooling (`examples/meas/`) — see `examples/CONTEXT.md`
- `priv/` → build outputs only: `priv/lib/` (flex-scanner shared objects) and `priv/plt/` (Dialyzer PLT); only their `.gitignore` files are tracked
- `Makefile`, `subdirs.mk`, `vsn.mk`, `configure`/`configure.ac`, `prebuild.skip` → app build/release driver, build subdir list (`src examples/simple examples/meas`), `MEGACO_VSN`, flex-scanner autoconf, bootstrap skips

## See Also
- `../CONTEXT.md` — how all `lib/<app>/` applications are structured, built and released.
- `../../CONTEXT.md` — repository-root build/release entry points (`configure`, `otp_build`, `otp_versions.table`).
- `../asn1/` — the ASN.1 compiler whose BER/PER back ends generate `src/binary/` codec modules (sibling app, read-only).
- `../et/`, `../runtime_tools/`, `../debugger/` — tracing/`ni` helpers named in `runtime_dependencies` (sibling apps, read-only).
