# erts/emulator/utils/ — emulator build-time code generators and helper tools

## Intent
Host-side tools that the emulator build (and the compiler's `beam_opcodes` build) run to *generate* sources and metadata that are intentionally not committed: opcode/dispatch tables, BIF/atom tables, allocator-type numbers, static driver/NIF registry, version/build-flag headers, and the preloaded-module blob.
None of these files is compiled into the emulator and none links against ERTS: they are build tooling, not runtime code.
Most of them run before any Erlang is available (bootstrap), so they are restricted to Perl 5 / POSIX shell; the three `escript` tools are the exceptions and are only used for cross builds or manual analysis.
All tools are invoked from the `erts/emulator/` build directory (cwd = `erts/emulator`), except `beam_makeops` which `lib/compiler/src/Makefile` calls by absolute path from `lib/compiler/src`.

## API Surface
Command-line contract → outputs. `$(TTF_DIR) = $(TARGET)/$(TYPE)/$(FLAVOR)` (e.g. `x86_64-pc-linux-gnu/opt/jit`), added to the include path as `-I$(TTF_DIR)`.

| Tool | Lang | Driven by (in `erts/emulator/Makefile.in` unless noted) | Inputs | Outputs |
|---|---|---|---|---|
| `beam_makeops` | Perl | `$(TTF_DIR)/OPCODES-GENERATED`; also `lib/compiler/src/Makefile: $(EGEN)/OPCODES-GENERATED` | `lib/compiler/src/genop.tab`, `beam/predicates.tab`, `beam/generators.tab`, plus per-flavor `beam/{emu,jit/<arch>}/ops.tab`, `predicates.tab`, `generators.tab` (emu additionally `macros.tab`, `instrs.tab`, `arith_/bif_/bs_/float_/map_/msg_/select_/trace_instrs.tab`) | emu mode: `beam_opcodes.c/.h`, `beam_hot.h`, `beam_warm.h`, `beam_cold.h`; jit mode: `beamasm_emit.h`, `beamasm_protos.h` (+ `beam_opcodes.c/.h`); `-compiler` mode: `beam_opcodes.erl`, `beam_opcodes.hrl` |
| `make_tables` | Perl | `$(TTF_DIR)/TABLES-GENERATED` | `beam/atom.names`, `beam/erl_dirty_bif.tab`, `beam/bif.tab` (DSL lines: `atom`, `bif`/`ubif`/`hbif`, `dirty-cpu`/`dirty-io[-test]`) | `erl_atom_table.c/.h`, `erl_bif_table.c/.h`, `erl_bif_list.h`, `erl_guard_bifs.c`, `erl_dirty_bif_wrap.c` |
| `make_alloc_types` | Perl | `$(TTF_DIR)/erl_alloc_types.h` | `beam/erl_alloc.types` (DSL: `allocator`/`class`/`type` + `+if/+ifnot/+else/+endif/+enable/+disable`) | `erl_alloc_types.h` (allocator/class/type number ranges) |
| `make_preload` | Perl | `$(TTF_DIR)/preload.c` (unix) / `$(TTF_DIR)/beams.rc` (win32) | `$(PRELOAD_BEAM)` list of `.beam` files | `-old`: C arrays `preloaded_<mod>[]` + `pre_loaded[]`; `-rc` (+`-windres` for mingw): Windows resource script |
| `make_driver_tab` | Perl | `$(TTF_DIR)/driver_tab.c` | `-nifs $(NIF_OBJS) $(STATIC_NIF_SYMBOLS)` `-drivers $(DRV_OBJS) $(STATIC_DRIVER_LIBS)` | `driver_tab.c` (`driver_tab[]`, `erts_static_nif_tab[]`, `erts_init_static_drivers()`) |
| `make_version` | Perl | `$(TARGET)/erl_version.h` | args `-o out $(SYSTEM_VSN) $(OTP_VERSION) $(VSN)$(SERIALNO) $(TARGET)` | `erl_version.h` (`ERLANG_OTP_RELEASE/_OTP_VERSION/_VERSION/_ARCHITECTURE`) |
| `make_compiler_flags` | Perl | `TMPVAR := $(shell ...)` at make parse time (runs on *every* make) | `-f CONFIG_H <erts/$(TARGET)/config.h>`, `-v CFLAGS/LDFLAGS` | `$(TTF_DIR)/erl_compile_flags.h` (`const char* erts_build_flags_*`) |
| `gen_git_version` | `sh` | `TMPVAR := $(shell utils/gen_git_version $(TARGET)/erl_git_version.h)` at make parse time | `git describe --match "OTP-[0-9]*" HEAD` under `$ERL_TOP` | `$(TARGET)/erl_git_version.h` (`ERLANG_GIT_VERSION`), value `na` commented out when no git checkout |
| `beam_emu_vars` | Perl | `make check_emu_registers` (manual, unix only) | `$(OBJDIR)/beam_emu*.S` (clang or gcc `-fverbose-asm`) | stdout: which register each `process_main` variable got; exit 1 if one spilled to `%rsp` |
| `beam_strip` | Perl | nothing (no Makefile references it) | `.beam` files named in argv, rewritten in place | removes the `LocT` and `CInf` IFF chunks |
| `find_cross_ycf` | escript | `YCF_EXECUTABLE_PATH` when `CROSS_COMPILING=yes` | none | prints the path of a usable `yielding_c_fun` in the bootstrap system (`$ROOT/bin` or `$ROOT/erts-<vsn>/bin`); exit 1 if not found |
| `count` | escript (no shebang) | manual (`escript erts/emulator/utils/count`) | `code:get_path()` `.beam` files | `./dis/*.dis` disassembly + `./dis/count` listing instruction frequencies sorted by count |
| `loaded` | escript (no shebang) | manual (`escript erts/emulator/utils/loaded`) | `code:lib_dir()` | count/bytes of BEAM files and number of words used when all modules are loaded |

Key command-line details:
- `beam_makeops`: `-emulator` (default) or `-compiler`; `-outdir DIR`; `-wordsize 32|64`; `-jit yes|no`; `-D<NAME>=<VALUE>` (e.g. `-DUSE_VM_PROBES=1`, and `-DARCH_32/ARCH_64` are derived from `-wordsize`); `-v`; remaining args are the `.tab` files.
- `make_tables`: `-src DIR`, `-include DIR`, `-dst yes|no` (from `$(DS_TEST)`, enables the `dirty-*-test` DSL lines), `-jit yes|no`.
- `make_alloc_types`: `-src FILE`, `-dst FILE`, plus bare `+<bool-var>` flags (from `$(ENABLE_ALLOC_TYPE_VARS)`).
- `make_preload`: `-old` (unix C source), `-rc` (Windows resource), `-windres` (mingw resource binary naming).
- `make_driver_tab`: `-o FILE`, then `-drivers ...` / `-nifs ...` groups; `foo.a:sym1:sym2` entries register statically linked NIFs from archives.

## Constraints
- Host tools only: runnable by the build *before* any Erlang exists; the Perl/`sh` tools must not shell out to Erlang, ERTS, or the target toolchain.
- `find_cross_ycf`, `count`, `loaded` need a running Erlang (bootstrap or built) — never make them a prerequisite for generating emulator sources.
- Only Perl 5, POSIX `sh`, and core tools (`sed`, `find`, `cp`, `test`, `git` optional) are assumed; the Makefile prefixes Perl invocations with `LANG=C`.
- Cross-compilation safe: tools only emit text; the only binary needed on the host (`yielding_c_fun`) is located via `find_cross_ycf` instead of being built for the target.
- Deterministic output: generation must not depend on locale, dictionary order, or wall-clock time. `make_alloc_types` honours `SOURCE_DATE_EPOCH`; `make_compiler_flags` and `gen_git_version` write the file only when the content actually changed.
- Windows-host support: paths go through `w32_path.sh` (see `make_preload`) and the win32 branch of `make_version` normalises `$CONFIG_SUBTYPE`.
- Never edit generated output; change the `.tab`/`.names`/`.types` inputs or the generator itself and rebuild.

## Design Decisions
- **Code generation instead of hand-written tables**: the BIF/atom tables, opcode dispatchers and allocator type numbers must stay mutually consistent with C code, tests and `erlang:system_info/1`; generating them from a small DSL (`bif.tab`, `atom.names`, `ops.tab`, `erl_alloc.types`) makes inconsistency a build error rather than a silent mismatch.
- **`.tab` DSL as the single source of truth for opcodes**: `lib/compiler/src/genop.tab` defines the *generic* opcode numbers shared with the compiler, while `beam/{emu,jit}/…/ops.tab` define the *specific* instructions and transformation rules. Because both the compiler and the emulator consume `genop.tab` through the same `beam_makeops`, opcode numbers can only change in a compatible way.
- **One generator, two front-ends**: `beam_makeops` serves the emulator (`-emulator`) *and* the compiler (`-compiler`, producing `beam_opcodes.erl/.hrl`) so the number/arity/name mapping can never drift between them; it also serves both flavors (`-jit yes` emits instruction-emitter glue for BeamAsm, `-jit no` emits interpreter block code).
- **Output in the per-build dir `$(TTF_DIR)=$(TARGET)/$(TYPE)/$(FLAVOR)` (and `$(TARGET)` for version headers), not the source tree**: the same sources generate jit and emu, several `TYPE`s (debug/valgrind/…) and several target architectures side by side; the source tree stays clean and free of machine-specific generated C (hence "missing" `beam_opcodes.c`, `erl_bif_table.c`, etc. in a clean checkout).
- **`make_preload` produces C/resource data rather than extern files**: keeps the preloaded modules inside the emulator binary/library so bootstrapping needs no external `.beam` files; it also strips the preloaded beams down to the chunks the loader needs (`Code`, `AtU8`, `ImpT`, `ExpT`, `StrT`, `FunT`, `LitT`, `Type`).
- **Version/build info compiled into the binary** (`erl_version.h`, `erl_git_version.h`, `erl_compile_flags.h`) so `erlang:system_info(system_version)`, `system_info(build_flags)` and crash dumps can report exactly how the emulator was produced.

## Known Issues / Gotchas
- A "missing" C file in a clean checkout is usually generated: read the table above before hunting for it.
- `beam_makeops` runs once per flavor with a different `-jit` value *and* a different `.tab` input set; jit and emu outputs for the same target live in sibling directories (`$(TARGET)/$(TYPE)/jit` vs `.../emu`) and must not be mixed.
- `beam_makeops` diagnostics: syntax/semantic errors are printed with `file(line)` context and abort the build; unused macros, predicates and generators are warned about unless declared with `%unused` in the `.tab` file.
- `make_compiler_flags` and `gen_git_version` are evaluated at make-parse time on **every** invocation (they are `$(shell …)` in the top of `Makefile.in`), so a broken Perl or a slow filesystem slows every `make` in `erts/emulator`.
- `erl_git_version.h` is deliberately used as a trigger for rebuilds: `$(ASMJIT_PCH_OBJ)` (the asmjit precompiled header) depends on `beam/erl_bif_info.c`, which includes both `erl_git_version.h` and `erl_compile_flags.h`.
- `erl_alloc_types.h` embeds the copyright year; without `SOURCE_DATE_EPOCH` it changes once a year and forces a rebuild.
- `beam_strip` is a leftover standalone utility with no Makefile caller — the preload path strips beams inside `make_preload` instead. Do not assume changing `beam_strip` affects the preloaded blob.
- `make_tables`' own usage comment still names `erl_am.c` (a JAM-era output); the generated set is the one listed in `erts/emulator/Makefile.in`.
- `count`/`loaded` have no shebang: invoke them as `escript erts/emulator/utils/{count,loaded}`, not `./count`.
- `beam_emu_vars` only runs on unix with an assembly listing (`beam_emu*.S`) and is a diagnostic, not a build step; `check_emu_registers` is `.PHONY`.
- Note that `erts/CONTEXT.md` refers to `make_emu_vars`; the actual tool is `beam_emu_vars`.

## Notes for Agents
- `../internal_doc/beam_makeops.md` is the authoritative description of the `.tab` DSL, generic vs. specific instructions, transformation rules and how to add/modify instructions; `../internal_doc/BeamAsm.md` covers the JIT emitter side. Read them before touching any `.tab` file.
- The `.tab` inputs and `beam/atom.names`, `beam/erl_alloc.types`, `beam/bif.tab`, `beam/erl_dirty_bif.tab` live in `../beam/` (owned by another agent) — this directory only contains the generators.
- After editing a generator or a `.tab` input, the corresponding `$(TTF_DIR)/{OPCODES,TABLES}-GENERATED` marker and the generated files under `$(TARGET)/…` are stale; run `make` in `erts/emulator` (or delete the marker) to regenerate.
- These tools are shipped in the source tree and are part of the public build contract: keep them runnable under a bare Perl 5 without CPAN modules (`File::Basename`/`File::Copy` are used, both core).

## Dependencies
- Perl 5 (with only core modules), POSIX `sh` (`gen_git_version`) and standard tools (`sed`, `find`, `test`); `git` is optional.
- Erlang/`escript` for `find_cross_ycf`, `count`, `loaded` and for `w32_path.sh` on Windows.
- `$(ERL_TOP)/lib/compiler/src/genop.tab` — the emulator build depends on the compiler application's opcode list.
- `yielding_c_fun` (built in `../../lib_src/yielding_c_fun`) for the `.ycf.h` generation that `find_cross_ycf` locates in cross builds.
- Windows only: `windres`/`rc.sh` and `w32_path.sh` for `make_preload -rc -windres` and `make_driver_tab`.

## Test Strategy
- No unit tests or golden files for these tools; validation is "the configured build succeeds and the emulator test suites pass" (`make test`, `erts/emulator/test`).
- Observable results of generation: `erlang:system_info(emu_flavor)`, `erlang:system_info(system_version)` (contains the git sha), `erlang:system_info(build_flags)`-style info from `erl_compile_flags.h`, and `erlang:system_info(allocator)`/alloc types from `erl_alloc_types.h`.
- `make check_emu_registers` (unix, JIT assembly) checks `process_main` register allocation.
- `utils/count` produces the static instruction-frequency table used when judging whether a new instruction is worth optimising.

## Routing Table
This node is a single flat directory — no child directories to delegate to.
- `../beam/` → the `.tab`/`.names`/`.types` generator inputs and all generated output consumers (read-only from here)
- `../internal_doc/` → `beam_makeops.md`, `BeamAsm.md`: the design documentation for what is generated
- `../../preloaded/src/` → `PRELOAD_BEAM` inputs for `make_preload`; strips beams with `beam_lib:strip/1` when refreshing them
- `../../../lib/compiler/src/` → `genop.tab` (input) and the Makefile that runs `beam_makeops -compiler` to produce `beam_opcodes.erl/.hrl`
- `../../Makefile.in` → all invocations, `$(TTF_DIR)`/`$(OBJDIR)` definitions and the generated-file dependency graph
- `../../lib_src/yielding_c_fun/` → the `yielding_c_fun` binary that `find_cross_ycf` locates
