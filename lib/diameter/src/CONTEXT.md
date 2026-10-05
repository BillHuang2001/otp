# diameter — sources and build (`src`)
## Intent
All Erlang sources of the `diameter` application, split into subdirectories by role (runtime core, dictionary compiler, transports, debug helpers, dictionaries) but compiled into a single flat `../ebin`.
`src/Makefile` is the app's real build driver; `modules.mk` is the single source of truth listing which module belongs where (and therefore what ends up in the generated `.app` file).
## API Surface
- `modules.mk` — partitions all sources and is included by `Makefile`, `../Makefile`-generated app files and by tests: `DICTS` (runtime dictionaries in `dict/`), `DICT_YRL` (`compiler/diameter_dict_parser`), `RT_MODULES` (runtime core, all in `base/` and `transport/`, **all** inserted into the generated `.app` `modules`), `CT_MODULES` (`compiler/`, excluded from `.app`), `INFO_MODULES` (`info/`, excluded from `.app`), `EXTERNAL_HRLS` (`diameter.hrl`, `diameter_gen.hrl`), `INTERNAL_HRLS` (private headers released per subdir), `BINS` (`diameterc`), `EXAMPLES`, `REGISTERED` (server names).
- `diameter.app.src` / `diameter.appup.src` — templates rendered by `Makefile` into `../ebin/diameter.app` / `.appup` via `sed` substitutions `%VSN%`, `%MODULES%`, `%COMPILER%`, `%INFO%`, `%REGISTERED%`; app version comes from `../vsn.mk` (`DIAMETER_VSN = 2.7.2`).
- `Makefile` targets: `opt` (default build), `strict` (`opt` with `-Werror`), `debug`/`TYPE=...`, `patch` (refuses to build if `git diff HEAD` is non-empty, then appends a `-<short-sha>` `PRE_VSN`), `app`, `dict`, `depend`, `info` (dumps all module lists), `plt`/`dialyze`, `clean`/`realclean`, and the release targets from `make/otp_release_targets.mk`.
- `Makefile` build facts: `EBIN = ../ebin`, `INCDIR = ../include`, `VPATH = .:base:compiler:transport:gen:info`, `RELSYSDIR = $(RELEASE_PATH)/lib/diameter-$(VSN)`.
  Compile flags include `-pa $(ABS_EBIN)` (so `include_lib("diameter/include/...")` resolves via the app name in the path), `-I $(INCDIR)`, `-I gen`, a `sys_pre_attributes` parse transform and `{attribute,insert,app_vsn,...}`.
  `MODULES = $(RT_MODULES) $(CT_MODULES) $(INFO_MODULES)` drives `depend.mk`, generated from the sources by `depend.sed`.
- `Makefile` code/dictionary generation: `gen/diameter_gen_%.erl|.hrl: dict/%.dia` runs `escript ../bin/diameterc -o gen -i $(EBIN) $<`; `gen/diameter_dict_parser.erl` comes from `compiler/diameter_dict_parser.yrl` via `$(ERLC) -Werror [+deterministic]`; `gen/diameter_gen_base_accounting.*` depends on the compiled `diameter_gen_base_rfc3588` beam and `gen/diameter_gen_acct_rfc6733.*` on `diameter_gen_base_rfc6733` (dictionary inheritance through beams).
- `release_spec` installs `bin/`, `ebin/`, `include/` (external + generated `.hrl`), `src/dict/*.dia`, and copies each subdirectory's `.erl`/internal `.hrl` sources into `$(RELSYSDIR)/src/<dir>/`.
## Constraints
- Add/rename/remove a module only by editing `modules.mk`; a module missing from `RT_MODULES` will not be in the `.app` `modules` list, and a module in `INFO_MODULES`/`CT_MODULES` must NOT be added to it (keeps `runtime_tools`/`syntax_tools` optional).
- Never hand-edit anything under `gen/` (all generated, git-ignored: `/diameter_dict_parser.erl`, `/diameter_gen*rl`); regenerate from `dict/*.dia` or `compiler/diameter_dict_parser.yrl`.
- `ebin/`, `depend.mk`, `otp.plt` and the generated app/appup files are build output — do not edit or commit.
- The `diameterc` escript requires the compiler modules to be on the code path (`-i $(EBIN)`), so a clean build must build `compiler/` before generating dictionaries.
- `strict` is the warning gate used in development; `opt` deliberately does not use `-Werror` so intentional warnings do not break the build.
## Design Decisions
- One flat `ebin` from many source subdirectories (`VPATH`) plus per-subdirectory release copying keeps the sources readable without turning subdirectories into separate applications — `../subdirs.mk` only has `SUB_DIRS = src doc`, so there is exactly one OTP application here.
- The three-way `modules.mk` partition (runtime / compiler / info) is what lets `diameter` declare only `stdlib`+`kernel` as hard dependencies while still shipping the dictionary compiler and debug helpers.
- Dictionary → Erlang code generation at build time (instead of interpreted dictionaries) keeps the codec fast and lets generated modules be plain `diameter_codec`-compatible modules.
## Known Issues / Notes for Agents
- `Makefile` uses `$(shell cd $(EBIN) && pwd)` for `ABS_EBIN` as a portable `realpath` — do not "modernize" it without checking make 3.80 compatibility.
- `patch` target creates an unofficial versioned build; it is not part of normal release flow.
- `.SECONDARY` keeps the intermediate `gen/*.erl|.hrl` and the generated yecc module, so `make clean` is required to force regeneration.
## Routing Table
- `base/` → handwritten runtime core: public `diameter` API, `diameter_app`, `diameter_codec`, service/config, watchdog/peer FSM, traffic, codec runtime, shared ETS/registry servers.
- `compiler/` → dictionary compiler (scanner, yecc grammar, `diameter_dict_util` semantics, `diameter_codegen`, public `diameter_make`).
- `transport/` → `diameter_transport` behaviour and the TCP/TLS, SCTP and Erlang-distribution transports plus their supervisors.
- `info/` → `diameter_dbg`/`diameter_info` inspection and tracing helpers (not in the `.app` `modules` list).
- `dict/` → the `.dia` dictionary sources compiled into `gen/diameter_gen_*` (`capup_rfc6737.dia` is present but not built).
- `gen/` → build output only (git-ignored); never edit or commit anything here.
- `modules.mk`, `Makefile`, `diameter.app.src`, `diameter.appup.src`, `depend.sed` → live at this level (no subdirectory).
## See Also
- `../CONTEXT.md` — the application as a whole; `../include/` — public headers; `../test/` — suites; `../bin/diameterc` — the compiler front-end escript; `../examples/` — sample code and dictionaries.
- `../../make/app_targets.mk`, `../../make/otp_subdir.mk`, `../../make/otp_release_targets.mk` — shared OTP build infrastructure used by this Makefile.
