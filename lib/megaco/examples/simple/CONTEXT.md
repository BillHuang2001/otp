# megaco — Simple MG/MGC Example

## Intent
Minimal, self-contained pair of Megaco/H.248 peers: a Media Gateway (`megaco_simple_mg`) and a Media Gateway Controller (`megaco_simple_mgc`).
Purpose is pedagogical and as a runnable/manual smoke-check of the `megaco` stack; it is also the payload driven by the megaco test suite and referenced from the published examples guide.
Built as part of the megaco app (`lib/megaco/subdirs.mk`: `SUB_DIRS = src examples/simple examples/meas`).

## API Surface
- `megaco_simple_mgc.erl` — the MGC (controlling side). `-behaviour(megaco_user)`.
  - Start/stop: `start/0`, `start/2` (`Trace, Debug`), `start/4` (`Mid, Config, Trace, Debug`, default `Mid = {deviceName, "controller"}`), `stop/0`, `stop/1`, `start_batch/0`, `start_batch/1`, `init_batch/3`.
  - `start/4` calls `megaco:start_user(Mid, [{user_mod, ?MODULE} | Config])` and then opens four listeners via `megaco_tcp:listen/2` / `megaco_udp:open/2` on `?megaco_ip_port_text` (TCP+UDP) and `?megaco_ip_port_binary` (TCP+UDP), using `megaco_pretty_text_encoder` / `megaco_binary_encoder` and `megaco_tcp` / `megaco_udp` as `send_mod`.
  - Handles only a single `serviceChangeReq` on the null context (`ServiceChangeReply` echoing `serviceChangeAddress`/`serviceChangeProfile`); anything else returns a `?megaco_not_implemented` `#'ErrorDescriptor'` with `discard_ack`.
- `megaco_simple_mg.erl` — the MG (controlled side). `-behaviour(megaco_user)`.
  - Start/stop: `start/0`, `start/3` (`MgcHost, Trace, Debug`), `start/4`, `stop/0`, `stop/1`, `start_batch/0`, `start_batch/1`, `init_batch/4`.
  - Per-transport starters: `start_tcp_text/2`, `start_tcp_binary/2`, `start_udp_text/2`, `start_udp_binary/2` — each registers a distinct Mid (`{deviceName, "gateway_tt"|"gateway_tb"|"gateway_ut"|"gateway_ub"}`) and `start/0` runs all four.
  - `start/4` does `megaco:start_user/2`, starts/connects the transport (`megaco_tcp:connect/2` or `megaco_udp:open/2` + `megaco_udp:create_send_handle/3`), `megaco:connect/4`, then `service_change/1` sends a `serviceChangeReq` (`restart`, `?megaco_cold_boot`, root termination, null context) with `megaco:call/3`.
  - `start_batch/1` parses command-line atoms of the form `{mgc_host, Host}`, `{trace, bool}`, `{debug, bool}` (defaults `mgc_host = local hostname`, `trace = false`, `debug = false`).
- Both modules implement the full `megaco_user` callback set: `handle_connect/2`, `handle_disconnect/3`, `handle_syntax_error/3`, `handle_message_error/3`, `handle_trans_request/3`, `handle_trans_long_request/3`, `handle_trans_reply/4`, `handle_trans_ack/4`, `handle_unexpected_trans/3`, `handle_trans_request_abort/4`.
- `modules.mk` — the module list consumed by the Makefile: `MG_MODULES = megaco_simple_mg`, `MGC_MODULES = megaco_simple_mgc`, `COMMON_MODULES =` (empty).
- `Makefile` — builds and installs the example (see build/run below).

## Build & Run
- Compilation: `make opt` (default) compiles the modules from `MG_MODULES`/`MGC_MODULES`/`COMMON_MODULES` with `EBIN = .`, so `.beam` files land directly in this directory (never hand-edited; ignored via the repo-root `.gitignore` `*.beam`).
- Compile flags: `-pa $(ERL_TOP)/lib/megaco/ebin`, `-I../../include`, `+nowarn_missing_doc +nowarn_missing_spec_documented +nowarn_deprecated_catch`; `TYPE=debug` adds `-Ddebug -W`.
- Key Makefile variables: `MGC_HOST` (defaults MG's `mgc_host` arg), `MG_INLINE_TRACE`/`MG_DEBUG` and `MGC_INLINE_TRACE`/`MGC_DEBUG` (toggle `{trace,_}`/`{debug,_}` start args), `WARN_UNUSED_WARS`, `DIAW_EH`/`DIAW_US`/`DIAW_UR`, `DIA_PLT` (`megaco_example_simple.plt`).
- Convenience targets: `make mg` and `make mgc` run `erl -noshell -pa <megaco_root>/ebin [-s megaco_filter] -s megaco -s megaco_simple_mg|megaco_simple_mgc start_batch <start args>`.
- Shell usage (see also `lib/megaco/doc/guides/megaco_examples.md`): `erl -pa ../../../megaco/ebin -s megaco_filter -s megaco`, then `megaco_simple_mgc:start().` and `megaco_simple_mg:start().` (MG and MGC must run on separate nodes/processes; MG connects to the MGC's well-known text/binary ports).
- Other targets: `clean`, `dclean`, `info`, `dialyzer_plt`, `dialyzer` (analysis output `megaco_example_simple.dialyzer_analysis`), no-op `docs`.
- Release install: `release_spec` depends on `opt`; `release_tests_spec` installs the `.erl` sources and `.beam` files into `$(RELEASE_PATH)/megaco_test/examples/simple` (`EXAMPLE_RELSYSDIR` is `$(RELEASE_PATH)/megaco_test/examples`); `release_docs_spec` only creates `$(RELEASE_PATH)/lib/megaco-$(VSN)/doc/examples/simple`. `VSN` comes from `../../vsn.mk` (`MEGACO_VSN`).

## Constraints
- A new example module must be added to the matching list in `modules.mk` (`MG_MODULES` / `MGC_MODULES` / `COMMON_MODULES`) — the Makefile derives `MODULES`, `ERL_FILES` and `TARGET_FILES` from it.
- `Makefile` here is checked in by hand (unlike `examples/meas/Makefile`, which is generated from `Makefile.in` by `configure`); do not add it to `lib/megaco/configure.ac`'s `AC_CONFIG_FILES`.
- Modules compile in place (`EBIN = .`); must stay runnable with only `-pa <megaco>/ebin`, i.e. depend on nothing outside `megaco`, `kernel`/`stdlib` and `inet`.

## Notes for Agents
- `megaco_simple_mg:start/4` is exported with a `???` comment in source — legacy/unused shell helper; the supported entry points are `start/0` and `start/3`.
- Both modules use `list_to_atom/1` on command-line input (`-compile({nowarn_possibly_unsafe_function, ...})`) and process-dictionary `debug` flag; debug output is `SIMPLE_MG: ...` / `SIMPLE_MGC: ...`.
- `MEGACO_ROOT_DIR` is computed as `$(shell (cd .. ; dirname \`pwd\`))` and points at `lib/megaco`, used as `-pa` for the `mg`/`mgc` targets.
- The MG's UDP path carries `BUGBUG` comments about obtaining the local port/socket; MGC address for UDP send-handle creation is hard-coded from the target host/port.

## See Also
- `../test/megaco_examples_SUITE.erl` (sibling — read-only, escalate writes to parent) — test case `simple/1` loads `megaco_simple_mg`/`megaco_simple_mgc` on two test nodes, starts MGC then MG, verifies `system_info(users)` and stops them; `lib/megaco/test/Makefile` adds `-pa ../../megaco/examples/simple` via `ERL_PATH`.
- `../meas/` — separate, heavier measurement example (its `Makefile` is configure-generated).
- `../../doc/guides/megaco_examples.md` — published documentation of these two examples.
- `../../CONTEXT.md` — megaco app overview and routing table.
