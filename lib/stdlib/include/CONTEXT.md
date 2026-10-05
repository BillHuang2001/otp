# stdlib/include — Public Headers

## Intent
The public, installed header directory of the `stdlib` application — the six `.hrl` files that ship in every OTP release under `$(RELSYSDIR)/include` and that all other OTP apps (and users) may include.
Content is either macro/behaviour switches (`assert.hrl`, `ms_transform.hrl`, `qlc.hrl`) or shared record/type definitions (`erl_bits.hrl`, `erl_compile.hrl`, `zip.hrl`).
Nothing here is generated and nothing here is build output — files are installed verbatim.

## API Surface

### assert.hrl (439 lines)
- Provides the assertion macros: `?assert/1,2`, `?assertNot`, `?assertMatch`, `?assertNotMatch`, `?assertEqual`, `?assertNotEqual`, `?assertException`, `?assertNotException`, `?assertError`, `?assertExit`, `?assertThrow` (every one has a 3-arg "comment" variant; `assertError/Exit/Throw` are thin wrappers over `assertException`).
- Success yields `ok`; failure raises `error` with `{MacroName, Info}` where `Info` is tagged tuples (`module`, `line`, `expression`, `expected`, `comment`, …).
- Widely consumed: 138 files repo-wide via `-include_lib("stdlib/include/assert.hrl")` (erts/emulator & erts/test ~39, lib/compiler ~35, lib/stdlib ~31, lib/kernel ~9, dialyzer, ssh, parsetools, inets, debugger, tools, sasl, public_key, eldap, common_test, asn1).
- Non-test consumer: `../../eunit/include/eunit.hrl` includes it (and defines `ASSERT`) so EUnit tests always have asserts enabled.
- User-facing documentation lives in `../doc/references/assert_hrl.md` (registered in `../doc/docs.exs`).

### ms_transform.hrl (22 lines)
- One directive: `-compile({parse_transform,ms_transform})`; enables `ets:fun2ms/1` and match-spec-in-a-fun syntax.
- Consumers (source): `../src/qlc_pt.erl`, `../runtime_tools/src/dbg.erl`, `../tools/src/cover.erl`, `../kernel/src/code_server.erl`, `../kernel/src/global.erl`, `../observer/src/crashdump_viewer.erl`, `../dialyzer/src/dialyzer_plt.erl`/`dialyzer_cplt.erl`/`dialyzer_iplt.erl`, `../parsetools/src/yecc.erl`, plus many `*_SUITE.erl` tests.

### qlc.hrl (22 lines)
- One directive: `-compile({parse_transform,qlc})`; required for the `qlc:q/1,2` query syntax.
- Consumers: `../src/qlc.erl` (self-include), user/example code such as `../../mnesia/doc/src/company.erl`, `../../mnesia/doc/src/company_o.erl`, and tests.
- `../src/erl_lint.erl` emits a warning when `qlc:q/1,2` is called without it (it checks `filename:basename(File) =:= "qlc.hrl"`), so the file name is load-bearing.

### zip.hrl (34 lines)
- Defines `#zip_file{name,info,comment,offset,comp_size}` and `#zip_comment{comment}`.
- Consumers: `../src/zip.erl` (which exposes the public types `zip_file()`/`zip_comment()` as these records) and `erts/preloaded/src/prim_zip.erl` (preloaded module).

### erl_compile.hrl (46 lines)
- Defines the legacy `#options{}` record of the old `erl_compile` API (include paths, outdir, defines, warning level, `output_type`, …).
- Consumers: `../src/erl_compile.erl`, `../compiler/src/compile.erl`, `../sasl/src/systools.erl`, `../snmp/src/compile/snmpc.erl`, `../snmp/src/compile/snmpc_mib_to_hrl.erl`, `../snmp/src/misc/snmp_verbosity.erl`, `../asn1/src/asn1ct.erl`, `../parsetools/src/yecc.erl`, `../parsetools/src/leex.erl`.

### erl_bits.hrl (35 lines)
- Defines the `#bittype{}` record plus the `bt_type/0`, `bt_unit/0`, `bt_sign/0`, `bt_endian/0` types used by binary-syntax type checking.
- Consumers: only `../src/erl_bits.erl` and `../src/erl_lint.erl` (both `-include_lib`); referenced in a comment from `../src/erl_parse.yrl`.

## Constraints
- Everything here is public API installed into `$(RELSYSDIR)/include`: macros and record fields cannot be retracted or renamed — third-party code and other OTP apps compile against them (removing a macro is caught early, see Known Issues in `../CONTEXT.md` history).
- Only files listed in `HRL_FILES` in `../src/Makefile` are installed; a new public header must be added to that variable.
- Internal-only headers do **not** belong here: `dets.hrl`, `erl_tar.hrl`, `otp_internal.hrl`, `json.hrl`, `swar_ascii.hrl` live in `../src/` and are installed into `$(RELSYSDIR)/src` (`INTERNAL_HRL_FILES`).
- Headers must be usable both as `-include_lib("stdlib/include/X.hrl")` and as plain `-include("X.hrl")` with `-I$(ERL_TOP)/lib/stdlib/include` — the latter is how `../compiler/src`, `../sasl/src`, `../parsetools/src` and `../src` itself consume them (`ERL_COMPILE_FLAGS += -I../../stdlib/include`), so filenames are part of the interface.
- `assert.hrl` must not depend on anything outside `kernel`/`stdlib` (it is used by erts tests and pre-bootstrap code) and its macro bodies must stay warning-free for consumers built with `-Werror +warn_deprecated_catch +warn_obsolete_bool_op`.
- Macro bodies use `(fun () -> ... end)()` wrappers and `__`-prefixed variables to avoid leaking bindings; assert macros cannot be nested.
- Only `assert.hrl` has an include guard (`ASSERT_HRL`); `erl_bits.hrl`, `erl_compile.hrl` and `zip.hrl` define records with no guard, so including them twice is a duplicate-record compile error.
- No generation, no build output, no `.gitignore`, no `ebin/` here — all six files are tracked source.
- Changing `assert.hrl` behaviour requires updating `../doc/references/assert_hrl.md` (and keeping the entry in `../doc/docs.exs` that publishes it).

## Notes for Agents
- The parse-transform headers are inert on their own: `ms_transform.hrl` needs the `ms_transform` module and `qlc.hrl` needs the `qlc` module available in the code path at compile time (the stdlib `ebin`).
- Including `qlc.hrl` also activates `ms_transform` (since `../src/qlc_pt.erl` includes it), so `ets:fun2ms` works inside qlc queries.
- `assert.hrl` gating: asserts are enabled unless `NOASSERT` is defined (which `NODEBUG` implies unless `DEBUG`/`ASSERT` is set); `ASSERT` overrides `NOASSERT`; when disabled every macro expands to the atom `ok`. `eunit.hrl` defines `ASSERT` before including this file.
- `#options{}` in `erl_compile.hrl` is the legacy `erl_compile` record still shared across compiler/sasl/snmp/asn1/parsetools builds — changing field names or order is a cross-application break.
- `#zip_file{}` is deliberately minimal; `zip.erl` keeps extra per-entry state in an internal `#zip_file_extra{}` record defined in `../src/zip.erl`, not here.
- `erts/preloaded/src/prim_zip.erl` is compiled during bootstrap with `STDLIB_INCLUDE = $(ERL_TOP)/lib/stdlib/include`, so `zip.hrl`'s record layout is an erts↔stdlib contract, and edits here affect the preloaded build first.
- `erl_bits.hrl` has no consumers outside stdlib, despite being installed as a public header.

## See Also
- `../CONTEXT.md` and `../src/` — implementation modules (`erl_compile.erl`, `erl_bits.erl`, `ms_transform.erl`, `qlc.erl`, `qlc_pt.erl`, `zip.erl`) and the internal-only headers.
- `../doc/references/assert_hrl.md` — user-facing documentation of `assert.hrl`; `../doc/docs.exs` registers it.
- `../../eunit/include/eunit.hrl` — re-exports the assert macros for EUnit.
- `../../../erts/preloaded/src/prim_zip.erl` — preloaded consumer of `zip.hrl`.
- `../src/Makefile` — `HRL_FILES` / `INTERNAL_HRL_FILES` and the install targets.
