# snmp Example ex1 — Agent Implementation Example

## Intent
Worked example of a *manual* SNMP agent implementation: the MIB `EX1-MIB` implemented by hand in Erlang (`ex1.erl`) instead of using the SNMP toolkit's default implementation.
It is the code behind the user guide `lib/snmp/doc/guides/snmp_impl_example_agent.md` ("Agent Implementation Example"), and it ships inside the release as documentation rather than as a runtime library.
Built by `lib/snmp/examples/Makefile` (via `subdirs.mk`: `SUB_DIRECTORIES = ex1 ex2`) as part of the `snmp` application build.

## API Surface
- `ex1.erl` — module `ex1`, 260 lines, no external dependencies beyond OTP.
  - `start/0` — `spawn(ex1, init, [])`; `init/0` (internal export) registers the `ex1_server` process and enters `loop/2`.
  - `my_name/1` (`get`) and `my_name/2` (`set, NewName`) — instrumentation functions for the `myName` scalar; return `{value, Name}` / `noError`.
  - `friends_table/3` — instrumentation function for the `friendsTable` table, clauses for `(get, RowIndex, Cols)`, `(get_next, RowIndex, Cols)`, `(is_set_ok, RowIndex, Cols)` and `(set, RowIndex, Cols)`.
  - State is held by the `ex1_server` process: `loop(MyName, Table)` with `Table` a sorted list of `{fIndex, fName, fAddress, fStatus}` 4-tuples; instrumentation callers send `{self(), Msg}` and block in `wait_answer/0` (replies are tagged `ex1_server`).
  - Table helpers: `get_cols/2`, `get_next_cols/2`, `end_of_table/1`, `add_one_to_cols/1`, `is_row_status_col_changed/1`, `get_row/1`, `get_next_row/1`, `table_get_row/2`, `table_get_next_row/2`, `table_delete_row/2`, `table_add_row/2`, `make_row/2`, `merge_rows/2`.
  - RowStatus constants: `?status_col` (4), `?active` (1), `?notInService` (2), `?notReady` (3), `?createAndGo` (4), `?createAndWait` (5), `?destroy` (6).
- `EX1-MIB.mib` — SNMPv1/SMIv1 MIB (`IMPORTS experimental FROM RFC1155-SMI`, `RowStatus FROM STANDARD-MIB`, `DisplayString FROM RFC1213-MIB`, `OBJECT-TYPE FROM RFC-1212`): root `example1 ::= { experimental 7 }`, scalar `myName`, `friendsTable`/`friendsEntry` indexed by `fIndex` with columns `fName`, `fAddress`, `fStatus` (RowStatus), and trap `fTrap` (ENTERPRISE example1, VARIABLES myName + fIndex).
- `EX1-MIB.funcs` — SNMP instrumentation (association) file pairing MIB objects with Erlang code: `{myName, {ex1, my_name, []}}.` and `{friendsTable, {ex1, friends_table, []}}.`.
- `EX1-MIBv2.mib` — SNMPv2 MIB variant (`SNMPv2-SMI`/`SNMPv2-CONF`/`SNMPv2-TC`, `MODULE-IDENTITY exampleModule`, `NOTIFICATION-TYPE`, `OBJECT-GROUP`/`NOTIFICATION-GROUP`/`MODULE-COMPLIANCE`); file is ISO-8859-1 encoded, not UTF-8.
- `EX1-MIBv2.funcs` — same two association entries as `EX1-MIB.funcs`.
- `Makefile` — the only build entry point in this directory.
  - `ERL_FILES = ex1.erl`, `MIB_FILES = EX1-MIB.funcs EX1-MIB.mib`, `BUILD_MIBS = EX1-MIB`.
  - `TARGET_FILES = EX1-MIB.bin ex1.$(EMULATOR)`; `build` (and `$(TYPES)`) build those; `clean` removes them plus `core`.
  - `release_docs_spec` installs `$(MIB_FILES)`, `$(ERL_FILES)` and `$(TARGET_FILES)` into `$(RELEASE_PATH)/lib/snmp-$(VSN)/doc/examples/ex1` by way of `EXAMPLE_RELSYSDIR = $(RELSYSDIR)/doc/examples`.
  - `VSN` comes from `include ../../vsn.mk` (`SNMP_VSN`); `docs` is an empty target.

## Constraints
- This is documentation example code: it is built and installed into the release *doc* tree (`doc/examples/ex1`) and its sources are installed verbatim so users can copy and compile them; it is not a runtime library of the `snmp` application and nothing in `snmp` depends on it.
- The module name `ex1` and the registered process name `ex1_server` are part of the documented transcript contract; renaming either requires updating `EX1-MIB.funcs`, `EX1-MIBv2.funcs` and `snmp_impl_example_agent.md`.
- Instrumentation functions must return the SNMP API result terms (`{value, V}`, `noError`, `{noError, Col}`, `{inconsistentValue, Col}`, `{inconsistentName, Col}`, `{noValue, noSuchInstance}`, `endOfTable`, `{[Col, RowIndex], Value}`) and must not raise.
- Compile flags are fixed by the `Makefile`: `-I../include`, `-Dversion="$(VSN)$(PRE_VSN)"`, `+'{parse_transform,sys_pre_attributes}'`, `+'{attribute,insert,app_vsn,$(APP_VSN)}'`, `-I$(ERL_TOP)/lib/stdlib`, `+nowarn_missing_doc +nowarn_missing_spec_documented +nowarn_deprecated_catch`, plus optional `MIBS_VERBOSITY`-derived `+'{verbosity,N}'`.
- Do not hand-edit `EX1-MIB.bin` or `ex1.beam`; both are build outputs produced in this directory.

## Known Issues
- `Makefile` lists only `EX1-MIB` in `MIB_FILES`/`BUILD_MIBS`, so `EX1-MIBv2.mib` and `EX1-MIBv2.funcs` are never compiled and never installed by any target; no other tracked file in the repository references `EX1-MIBv2`, and the guide documents only `EX1-MIB`.
- `-I../include` points at `lib/snmp/examples/include`, which does not exist (the examples use no `.hrl` files), making the flag a no-op inherited from the `ex2` Makefile.
- `-Dversion="..."` is passed at compile time but `ex1.erl` never references `?version`.
- `ex1.erl` has no `-author` attribute, while the copy inlined in `snmp_impl_example_agent.md` carries `-author('dummy@flop.org')`; the guide's code sample and this file can drift apart.

## Notes for Agents
- `EX1-MIB.bin` is produced in this directory by the generic `.mib.bin` suffix rule in `make/otp.mk.in`, which invokes `erlc -pa $(SNMP_TOOLKIT)/ebin -I $(SNMP_TOOLKIT)/priv/mibs` with the SNMP MIB compiler; the target here is a bare `EX1-MIB.bin`, not `$(SNMP_BIN_TARGET_DIR)/EX1-MIB.bin`.
- The MIB compiler looks for the association file as `<MibName>.funcs` relative to the current directory (`snmpc.erl:parse/1` → `{file, FileName ++ ".funcs"}`); a missing file is silently ignored and yields the default implementation, so `EX1-MIB.funcs` must sit next to `EX1-MIB.mib` to take effect. `MIB_FILES` exists mainly to copy both files into the release doc tree.
- There are no Common Test suites here (`lib/snmp/test/` owns SNMP testing; it keeps its own copy of `EX1-MIB.mib` under `test/snmp_test_data/`).
- `MIBS_VERBOSITY` is the only tunable make variable (adds `+'{verbosity,N}'` to the MIB compile).

## Dependencies
- Build-time: `$(ERL_TOP)/make/target.mk`, `$(ERL_TOP)/make/$(TARGET)/otp.mk`, `$(ERL_TOP)/make/otp_release_targets.mk`, `../../vsn.mk`, and a built `snmp` application (the MIB compiler in `snmp/ebin`) plus `stdlib` on the compiler path.
- Runtime usage (user-driven, per the guide): inside a running `snmp` node — `snmpc:compile("EX1-MIB")`, `snmpa:load_mibs(snmp_master_agent, ["EX1-MIB"])`, then `ex1:start()`.

## See Also
- `../CONTEXT.md` — the SNMP examples build (`Makefile`, `subdirs.mk`).
- `../ex2/` — the sibling manager-side example.
- `../../doc/guides/snmp_impl_example_agent.md` — the user guide that documents this example (MIB, association file, transcripts, trap sending).
- `../../test/snmp_test_data/` — test copies of `EX1-MIB.mib` (sibling outside this node — read-only).
