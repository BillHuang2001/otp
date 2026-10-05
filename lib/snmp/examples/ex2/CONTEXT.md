# snmp example ex2 — Manager Implementation Example

## Intent
Worked, shipped example for the `snmp` application demonstrating two things: how to implement a simple SNMP manager on top of the `snmpm` manager component, and how to write agent test cases using such a manager.
It is the code referenced by the user guide `../doc/guides/snmp_impl_example_manager.md` ("Manager Implementation Example", the _ex2_ example).
It is a teaching artefact, not library code: it is compiled here and published into the release doc tree, and is not part of `snmp`'s `ebin/` or any application module list.

## API Surface
`snmp_ex2_manager.erl` (module `snmp_ex2_manager`) — `gen_server` + `snmpm_user` behaviour; a self-contained example manager.
- Lifecycle: `start_link/0`, `start_link/1` (options list), `stop/0`.
  - Recognized options: `port` (5000), `engine_id` ("mgrEngine"), `max_message_size` (484), `mibs` ([]), `versions` ([v1,v2,v3]), `dir` (process cwd).
  - `do_init/1` writes a manager config with `snmp_config:write_manager_config/3`, starts the manager with `snmpm:start_link/1`, then registers itself via `snmpm:register_user(?MODULE, ?MODULE, self())`.
- Client API (each wraps the corresponding `snmpm` function, guarding with `catch`, called via `gen_server:call(..., infinity)`):
  - `agent/2` → `snmpm:register_agent/3` (register a target agent + its config).
  - `sync_get/2`, `sync_get_next/2`, `sync_get_bulk/4`, `sync_set/2` → `snmpm:sync_get2/3`, `sync_get_next2/3`, `sync_get_bulk2/5`, `sync_set2/3`.
  - `oid_to_name/1` → `snmpm:oid_to_name/1`.
- `snmpm_user` callbacks demonstrated (the point of the example): `handle_error/3`, `handle_agent/5`, `handle_pdu/4`, `handle_trap/3`, `handle_inform/3`, `handle_report/3`, `handle_invalid_result/3` — each just forwards `{snmp_callback, Tag, Info}` to the *parent* pid captured at `start_link` (`#state{parent}`) and returns `ignore`/`ok`.
  - `handle_info/2` receives `{snmp_callback, Tag, Info}` and pretty-prints it with `io:format/2` via `handle_snmp_callback/2`.
- Internal helpers: `parse_opts/1`, `get_opt/3`, `write_config/2`, `start_manager/1`, `register_user/0`, `call/1`, `cast/1`, `error_msg/2`, local `error/1` (throws `{error, Reason}`).

`snmp_ex2_simple_standard_test.erl` (module `snmp_ex2_simple_standard_test`) — minimal driver showing how to exercise an agent through the example manager.
- `start/0` (resolves localhost), `start/1` (agent address), `start/3` (`Mibs`, `AgentAddr`, `AgentConfig`); the 3-arity entry starts the manager, registers the agent, runs `simple_standard_test/1`, stops the manager.
- Command list is a `#command{tag, desc, cmd, verify}` record list; `commands/1` runs each `cmd` closure, feeds the result to `verify`, and prints `ok`/`error` (no assertions, no exit status).
- The demonstrated cases: `get-next` from successively deeper OIDs (all must reach `sysDescr`), `get [sysDescr,0]`, and `get` on a non-existent OID expecting `noSuchInstance`/`noSuchName`.
- Support code: `gn/2` / `g/2` shorthands, `verify_vbs/2,3` + `verify_vb/2` + `verify_vb_value/3` (check varbind name via `snmp_ex2_manager:oid_to_name/1` and its type), `std_mib/1` + `std_dir/0` loading `STANDARD-MIB` from `code:priv_dir(snmp)/mibs`.

## Makefile
- Includes `$(ERL_TOP)/make/target.mk`, `make/$(TARGET)/otp.mk`, `../../vsn.mk` (`VSN = $(SNMP_VSN)`), and `$(ERL_TOP)/make/otp_release_targets.mk`.
- `MODULES = snmp_ex2_manager snmp_ex2_simple_standard_test`; `build` compiles the `ERL_FILES` to `$(EMULATOR)` beams (`$(TYPES): build`, so `make opt` / `make debug` work).
- Compile flags: `-pa $(ERL_TOP)/lib/snmp/ebin -I../include -Dversion="$(VSN)$(PRE_VSN)" -I$(ERL_TOP)/lib/stdlib` plus the `sys_pre_attributes` parse transform and `+nowarn_missing_doc +nowarn_missing_spec_documented +nowarn_deprecated_catch` (same pattern as the other `snmp` Makefiles).
- `release_docs_spec` installs **both** the `.erl` sources and the compiled beams into `$(RELEASE_PATH)/lib/snmp-$(VSN)/doc/examples/ex2` (`EXAMPLE_RELSYSDIR` + `EX2_RELSYSDIR`) — examples ship as documentation.
- Reached from `../subdirs.mk` (`SUB_DIRECTORIES = ex1 ex2`) and `../Makefile` (`otp_subdir.mk`); `examples` is itself in `../../subdirs.mk`.

## Constraints
- Documentation example: do **not** move these modules into `../../src/`, and do not add them to `snmp`'s module list or build order.
- The warning suppressions are intentional — the example deliberately carries no `-doc`/`-spec` and uses `catch` for brevity; do not "fix" them by adding docs or specs.
- Both modules need `-compile({no_auto_import,[error/1]})` because they define a local `error/1` shadowing the auto-imported BIF.
- The example must keep compiling with `-pa $(ERL_TOP)/lib/snmp/ebin`: `snmp` must be built before this directory is built.
- Sources and beams here end up in the installed release doc tree, so file names/module names are part of the published documentation.

## Known Issues
- The user guide `../doc/guides/snmp_impl_example_manager.md` advertises arities that do not exist: `agent/3`, `sync_get/3`, `sync_get_next/3`, `sync_get_bulk/5`, `sync_set/3`. The module actually exports only `agent/2`, `sync_get/2`, `sync_get_next/2`, `sync_get_bulk/4`, `sync_set/2` — the manager handle is implicit (local registered `gen_server`), so there is no extra `Server` argument.
- `handle_report/3` mislabels reports: it forwards with the `handle_inform` tag, so a report is printed as `*** Received INFORM ***` and the `handle_report` clause of `handle_snmp_callback/2` is unreachable.
- The header comment in `snmp_ex2_manager.erl` says "This module examplifies how to write test suites for your SNMP agent", which is a copy of the comment in the test module and does not describe the manager module.

## Test Strategy
- None of this is a Common Test suite: `snmp_ex2_simple_standard_test` is a hand-rolled driver invoked manually (`start/0` talks to an agent on localhost with community `"public"` exposing `STANDARD-MIB`); it prints results rather than asserting, and nothing in the repo or in `snmp`'s test suites references these modules.
- Static analysis lives in this Makefile: `make dialyzer_plt` builds `snmp_example_ex2.plt` over apps `snmp erts kernel stdlib crypto mnesia runtime_tools compiler`; `make dialyzer` runs Dialyzer over this directory using that PLT; `make dclean` removes the PLT and the `.dialyzer_analysis` output.
- Optional stricter Dialyzer flags via `DIAW_EH=true` (`-Werror_handling`), `DIAW_US=true` (`-Wunderspecs`), `DIAW_UR=true` (`-Wunmatched_returns`).
- The repo-wide `../../../../scripts/run-dialyzer` operates per OTP *application* and does not cover `examples/`, so these targets are the only Dialyzer coverage for these modules.
- Only real check available in CI-style automation is compilation (`make`/`make opt` after `snmp` is built) — there is no test target here.

## Notes for Agents
- Running the example creates its manager config/database files under the `dir` option, which defaults to the current working directory — pass `{dir, SomeDir}` instead of running from a source checkout root.
- `../ex1/` is the sibling agent-side example (`ex1.erl` + `EX1-MIB.mib`/`EX1-MIBv2.mib`) and is a natural target to run `snmp_ex2_simple_standard_test` against; treat it as read-only (writes escalate to the parent node).
- The `version` macro passed as `-Dversion=...` is not referenced by either source file.
- `snmp_ex2_manager` registers itself as a *local* process named after the module (`{local, ?MODULE}`) and registers the SNMP user `snmp_ex2_manager`, so only one instance can run per node.

## See Also
- `../CONTEXT.md` — the `examples/` build coordinator and its `SUB_DIRECTORIES`.
- `../ex1/` — agent implementation example (sibling, read-only).
- `../../doc/guides/snmp_impl_example_manager.md` — the guide that documents this example.
- `../../src/manager/` — the real `snmpm` manager implementation these examples call.
