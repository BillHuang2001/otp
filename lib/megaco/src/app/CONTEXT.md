# megaco/src/app — Public API + application resources

## Intent
The user-facing surface of the `megaco` application: the single public API module `megaco.erl`, the application resource templates (`megaco.app.src`, `megaco.appup.src`) and the app-internal shared header `megaco_internal.hrl`.
This is the only `src/` subdirectory that produces a *public* Erlang module; every other subdir (`engine/`, `text/`, `flex/`, `binary/`, `tcp/`, `udp/`) holds internal implementation modules.
It also owns the per-directory build glue (`Makefile`, `modules.mk`, `megaco.mk`, `depend.mk`) — but only for this directory's one module.

## API Surface
- `megaco.erl` (3190 lines) — the whole public Erlang API of the app; it contains no protocol logic, it only validates/forwards to `megaco_messenger`, `megaco_config`, `megaco_digit_map`, `megaco_sdp`, `megaco_binary_term_id` (`megaco_sdp`, `megaco_digit_map`, `megaco_config` and `megaco_messenger` live in `../engine/`; `megaco_binary_term_id` in `../binary/`).
  - Module doc banner is `-moduledoc "Main API of the Megaco application"` (line 29); it is generated as the `megaco` reference page.
  - Exported groups (lines 41–97): lifecycle `start/0 stop/0 start_user/2 stop_user/1`; info `info/0,1 user_info/1,2 update_user_info/3 conn_info/1,2 update_conn_info/3 system_info/0,1`; connection/transaction `connect/4,5 disconnect/2 call/3 cast/3 cancel/2 process_received_message/4,5 receive_message/4,5`; encoding `encode_actions/3 token_tag2string/1,2,3 encode_sdp/1 decode_sdp/1 encode_binary_term_id/2 decode_binary_term_id/2 get_sdp_record_from_PropertyGroup/2`; digit maps `parse_digit_map/1 eval_digit_map/1,2 report_digit_event/2 test_digit_event/2`; versions `versions1/0 versions2/0 print_version_info/0,1`; trace `enable_trace/2 disable_trace/0 set_trace/1 ms/0 nc/0,1 ni/0,1`; events `report_event/4,5`; test helpers `test_request/5 test_reply/5`; stats `get_stats/0,1,2 reset_stats/0,1`; timestamp formatting `format_timestamp/1,2 format_short_timestamp/1,2 format_long_timestamp/1,2 formated_timestamp/0 formated_short_timestamp/0 formated_long_timestamp/0`.
  - `-export_type` (lines 99–121): `void, mid, receive_handle, conn_handle, action_request, action_reply, error_desc, transaction_reply, protocol_version, digit_map_value, digit_map_kind, digit_map_event, digit_map_letter, property_parm, property_group, property_groups, sdp, trace_level`; most of these alias types in `megaco_encoder`/`megaco_user` (in `../binary/`, `../engine/`).
  - 19 exported functions carry `-doc false` (internal/utility): `encode_binary_term_id/2`, `decode_binary_term_id/2`, `get_sdp_record_from_PropertyGroup/2`, `ms/0`, `nc/0,1`, `ni/0,1`, `report_event/4,5`, and all `format*/formated*_timestamp` functions — they exist for tracing, self-tests and `megaco_user`-style debugging, not as supported API.
- `megaco.app.src` — application resource template: `vsn` is `"%VSN%"` (substituted by `make`), a *hand-maintained* `modules` list (65 entries, covering the generated ASN.1/Yecc modules from `../binary/` and `../text/` that do not exist in git), `registered = [megaco_config, megaco_monitor, megaco_trans_sup, megaco_misc_sup, megaco_sup]`, `mod = {megaco_sup, []}`, `applications = [stdlib, kernel]`, `env = []`.
- `megaco.appup.src` — release-handling template, also `"%VSN%"`-substituted; the actual instructions are just `{<<"3\\..*">>, [{restart_application, megaco}]}` and `{<<"4\\..*">>, [{restart_application, megaco}]}` (both directions), so every upgrade/downgrade restarts the whole application.
- `megaco_internal.hrl` (203 lines) — *internal* header, not installed into `include/`: `?APPLICATION`, the `#conn_data{}` connection-state record, `#remote_conn_data{}`, the log/report macros (`?megaco_info/warning/error`, `?megaco_ereport/wreport/ireport`, `?report_important/verbose/debug/trace`), `?default_user_callback_extra`, and the compile-time-conditional `?d/2` debug macro.
- Build glue: `modules.mk` (`MODULES = megaco`, plus `EXTERNAL_HRL_FILES` = the 5 `../../include/*.hrl` and `INTERNAL_HRL_FILES = megaco_internal.hrl`), `megaco.mk` (compile-flag knobs, see Design Decisions), `depend.mk` (`$(EBIN)/megaco.beam` depends on `megaco.erl` + `megaco_internal.hrl`), `Makefile` (targets `opt`, `clean`, `docs` (empty), `info`, `release_spec`, `release_docs_spec`).
- Generated build outputs (git-ignored, do not edit): `../../ebin/megaco.app`, `../../ebin/megaco.appup`, `../../ebin/megaco.beam` (from `%VSN%` `sed` substitution and compilation).

## Constraints
- `megaco.erl` must stay free of protocol logic — new behaviour belongs in `../engine/`'s `megaco_messenger`/`megaco_config` and is only re-exported here.
- Adding/removing/renaming any module anywhere under `src/` requires editing the `modules` list in `megaco.app.src` by hand (it is not derived from the build); forgetting this breaks `versions1/0`, release upgrade and `application:get_key(megaco, modules)`.
- `megaco_internal.hrl` must physically stay at `src/app/megaco_internal.hrl`: 19 modules across `../engine/`, `../binary/`, `../flex/`, `../tcp/`, `../udp/` and `../../test/megaco_config_SUITE.erl` reach it via `-include_lib("megaco/src/app/megaco_internal.hrl")`, and `release_spec` installs it to `$(RELEASE_PATH)/lib/megaco-$(VSN)/src/app/` precisely so that path resolves in an installed release.
- `megaco_internal.hrl` field additions are doubly constrained: the header itself warns "N.B. Update megaco_config when a new field is added" (for `#conn_data{}` and `#remote_conn_data{}`), so a change here is a cross-module change to `../engine/megaco_config.erl`.
- The `%VSN%` placeholder is the only version mechanism in the `*.src` files — never hard-code a version string, and never hand-edit `../../ebin/megaco.app`/`megaco.appup`.
- Keep the `EXTERNAL_HRL_FILES` list in `modules.mk` in sync with `../include/`: it is what gets promoted into the installed `include/` directory, so removing an entry silently drops a public header from releases.
- The `-include("megaco_internal.hrl")` in `megaco.erl` sits at line 1236, *after* ~1100 lines of `-doc`/`-type`/`-spec` declarations — do not move code above it expecting macros to be available.

## Design Decisions
- The header is `-include`d (relative) by `megaco.erl` but `-include_lib`d by all other subdirs, because only `app/` is compiled in the same source directory; the other subdirs rely on `megaco` already being resolvable in the code path.
- `megaco.mk` centralises the compile-time feature flags for *this* directory, all off by default: `MEGACO_TRACE=io` → `-Dmegaco_trace_io` (the `?report` macro writes straight to stdout instead of calling `megaco:report_event/5`), `MEGACO_EXTENDED_TRACE`, `USE_MEGACO_TEST_CODE` (defines `MEGACO_TEST_CODE=mona_lisa_spelar_doom`, read by `../engine/megaco_messenger.erl`, `megaco_messenger_misc.erl`, `megaco_config.erl`), `MEGACO_DEBUG` (enables `?d/2`), `MEGACO_PARSER_INLINE` (on unless set to `false`; consumed by `../text/megaco_text_parser_v*.hrl`), `WARN_UNUSED_WARS`, and `ERL_DETERMINISTIC` (passes `+deterministic` to erlc **and** sets `YRL_FLAGS`/`XRL_FLAGS` for the generated parsers).
- The version shown by `versions1/0` is a compile-time attribute, not a runtime lookup: `MEGACO_APP_VSN_COMPILE_FLAGS` inserts `{attribute,insert,app_vsn,$(APP_VSN)}` via the `sys_pre_attributes` parse transform (so `compiler` must be available at build time), while `versions2/0` uses `application:get_key/2` instead.
- `megaco.erl` is compiled with `-pa $(ERL_TOP)/lib/et/ebin -pa $(ERL_TOP)/lib/megaco/ebin`; tracing is built on `dbg` (runtime_tools) plus `et_selector` (et), `set_trace/1` being just `et_selector:change_pattern(et_selector:make_pattern({?MODULE, Level}))`.
- `enable_trace/2` and `disable_trace/0` deliberately go through the traced `report_event/5` (a no-op clause returning the atom `hopefully_traced` guarded the tracer) so trace start/stop are themselves observable; `report_event/4` performs an explicit `?MODULE:report_event/5` external call for the same reason.
- `runtime_dependencies` lists `stdlib, runtime_tools, kernel, et, erts, debugger, asn1` although `applications` is only `[stdlib, kernel]`: `runtime_tools`/`et` are needed by tracing, `debugger` by the `ni/0,1` interpret-load helper (`int:ni/2`), and `asn1` is a build-time/generated-module requirement (no `asn1:` call exists in `src/`).
- `docs:` in this `Makefile` is intentionally empty — the ExDoc build lives in `lib/megaco/doc/`.
- `-compile([{nowarn_possibly_unsafe_function, {file, consult, 1}}])` silences the SCS warning for `ms1/0`, which `file:consult/1`s `ebin/megaco.app` to fetch the module list.

## Known Issues
- `megaco.appup.src` still carries a large ASCII upgrade-graph comment (`3.4.3` … `3.18`) left over from when appup instructions were enumerated per release; it is now stale relative to the 4.x `VSN` and the regex-only instruction list, so it should not be used as an upgrade reference.
- The `-doc false` exports (`ms`, `nc`, `ni`, timestamp formatters, …) are reachable by any caller and print to `io` / compile or interpret modules at runtime; they are debug helpers misusable as API.
- The `format*/formated*_timestamp` family has both spellings ("formated" without the second t) exported — an easy source of compile errors; `formated_*_timestamp/0` are only wrappers around the `format_*` versions.
- `nc/0,1` calls `c:nc/2` (compiler) and `ni/0,1` calls `int:ni/2` (debugger), yet `compiler` is not listed in `runtime_dependencies` while `debugger` is.

## Notes for Agents
- Do not read `megaco.erl` in full (~106 KB): lines 1–1235 are documentation/type declarations, the include is at 1236, and the function bodies then run in this order — lifecycle/users/conns/info (1240–1550), connect/disconnect (1554–1695), call/cast/test_request/test_reply (1695–1905), stats (1908–2000), cancel/process|receive_message (2000–2150), encode_actions/token_tag2string (2150–2235), digit maps (2235–2360), binary term id + SDP (2361–2480), versions/print_version_info (2480–2715), `ms/nc/ni` load helpers (2715–2845), tracing (2844–3090), timestamps (3090–3190).
- `grep -n '^-doc\|^-spec\|^-export'` is the fastest way to navigate the module; the `-doc` blocks are the authoritative behavioural description (several are long — e.g. `conn_info/1`, `system_info/0` enumerate every info key and its default).
- Changing a documented function's behaviour here almost always means changing `megaco_messenger`/`megaco_config` in `../engine/`; check `git log -p -- lib/megaco/src/app/` for prior coordinated changes before assuming this file is self-contained.
- `test_request/5` and `test_reply/5` are public-but-test-only entry points kept in the API precisely so test suites can compose+encode a message without a live connection.

## Dependencies
- Build: Erlang compiler plus the `sys_pre_attributes` parse transform; `make`; the top-level `configure` for the app (`lib/megaco/configure`, itself run by the root `configure`).
- Build order: `src/subdirs.mk` builds `app` first (`SUB_DIRECTORIES = app engine text flex binary tcp udp`), because `megaco.beam` is on the code path used to compile the rest.
- Runtime: only `kernel` + `stdlib` are hard requirements; `runtime_tools`, `et`, `debugger`, `asn1` are declared as runtime dependencies for optional tracing/load helpers.
- `lib/megaco/prebuild.skip` keeps the two `megaco_flex_scanner*` beams out of the prebuilt bootstrap set, so those are always rebuilt from source.

## Test Strategy
- No test suites live in this directory; the app's Common Test suites are in `lib/megaco/test/` (`megaco_config_SUITE` etc.), which also `-include_lib`s `megaco_internal.hrl` from here.
- The public API is exercised indirectly by those suites via `megaco:start_user/2`, `connect/4`, `call/3` and the `test_request/5`/`test_reply/5` helpers; there is no unit test of the pure forwarding functions.
- `make test` / `make release_tests` for the app are driven from `lib/megaco/Makefile`, not from this `Makefile` (which has `clean/opt/docs/info` only).

## See Also
- `../engine/` — `megaco_messenger`, `megaco_config`, `megaco_sup` and the callback behaviour `megaco_user`; the real implementation behind every function here (read-only from this node).
- `../binary/` — encoding modules whose types (`megaco_encoder:*`) back the `-type` aliases exported here.
- `../text/`, `../flex/` — text/compact encoders and the flex scanner that produce generated modules listed in `megaco.app.src`.
- `../tcp/`, `../udp/` — transports; their modules receive messages by calling `megaco:process_received_message/5`.
- `../../include/` — the public headers installed by `release_spec` (`megaco.hrl`, `megaco_message_v1..v3.hrl`, `megaco_sdp.hrl`).
- `../../doc/` — ExDoc sources; `../../test/` — Common Test suites.
- `../../vsn.mk` — defines `MEGACO_VSN`/`APP_VSN` used by `%VSN%` substitution and the `app_vsn` attribute.
