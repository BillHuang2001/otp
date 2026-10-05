# erts/emulator/beam/ — BEAM emulator core (VM implementation)

## Intent
The flat, single-directory core of the BEAM virtual machine: ~107 `.c` files and ~97 `.h` files that implement data types, BIFs, GC, scheduling, allocators, ETS, distribution, code loading, tracing, NIF/driver support, timers and ports.
Everything here is compiled and linked into the emulator executable (`$ERL_TOP/bin/$(TARGET)/beam.smp`) and into the archive `libbeam.a`.
Flavor/architecture-specific code lives in the child dirs `jit/` and `emu/` (see Routing Table); this directory holds the flavor-independent parts plus the codegen *inputs* shared by both.

## API Surface
### Public C headers (installed to `$RELEASE_PATH/usr/include` and `erts-$(VSN)/include`)
- `erl_driver.h` — driver developer API (threads, locks, memory, binary, async, port I/O).
- `erl_nif.h`, `erl_nif_api_funcs.h` — NIF API (version history in the header maps API version → OTP release).
- `erl_drv_nif.h` — types/decls shared between `erl_driver.h` and `erl_nif.h`.
- `driver_int.h` is per-OS and lives in `../sys/<ostype>/`, not here.
- `erl_internal_test.h` — internal only, installed to `erts-$(VSN)/include/internal`.
These four headers (plus per-OS `driver_int.h`) are the ONLY stable ABI exposed from this directory; everything else is private to the emulator.

### Generated-code INPUTS kept here (hand-maintained, not generated)
- `atom.names`, `bif.tab`, `erl_dirty_bif.tab` → generated into `$(TTF_DIR)` by `../utils/make_tables`: `erl_bif_table.c|h`, `erl_bif_list.h`, `erl_atom_table.c|h`, `erl_guard_bifs.c`, `erl_dirty_bif_wrap.c` (further consumed by `../utils/beam_makeops`).
- `erl_alloc.types` → `$(TTF_DIR)/erl_alloc_types.h` via `../utils/make_alloc_types` (defines `ErtsAlcType_t` values and type→strategy mapping).
- `predicates.tab`, `generators.tab` → concatenated with the flavor tables (`../beam/jit/$(JIT_ARCH)/{predicates,generators}.tab` for `FLAVOR=jit`, `../beam/emu/{predicates,generators}.tab` for `FLAVOR=emu`) and fed to `../utils/beam_makeops`, producing `$(TTF_DIR)/beam_opcodes.c|h`, `beam_hot.h`, `beam_warm.h`, `beam_cold.h` (the instruction table + dispatch code shared by interpreter and JIT).
- `erlang_dtrace.d` → `$(TARGET)/erlang_dtrace.h` when DTrace is enabled.
`$(TTF_DIR)` = `erts/emulator/$(TARGET)/$(TYPE)/$(FLAVOR)`.

### Sources rewritten by the yielding-C compiler (YCF)
`erl_map.c` → `$(TTF_DIR)/erl_map.ycf.h`, `utils.c` → `utils.ycf.h`, `erl_db.c` → `erl_db_insert_list.ycf.h`.
Each `.c` `#include`s its own generated `.ycf.h` near the bottom; the generator binary is `../lib_src/yielding_c_fun`.

### What it contributes to the build
`../Makefile.in` globs `BEAM_SRC=$(wildcard beam/*.c)` (dependency/compdb scanning) and lists the objects explicitly in `COMMON_OBJS` (loader/JIT-shared: `beam_common`, `beam_bif_load`, `beam_bp`, `beam_catches`, `beam_debug`, `beam_load`, `beam_opcodes`, `beam_ranges`, `beam_transform_helpers`, `code_ix`), `RUN_OBJS` (everything else here), `LTTNG_OBJS` (`erlang_lttng.c`), plus `OS_OBJS`/`NIF_OBJS`/`DRV_OBJS` from other dirs.
Outputs: `$ERL_TOP/bin/$(TARGET)/beam.<flavor>` and the archive `EMULATOR_LIB` (`libbeam.a`, `libbeam.lib` on Windows) installed to `erts-$(VSN)/lib`.
There are currently **no `.cpp` files directly in `beam/`** (the `BEAM_CPP_SRC` glob is for `jit/`).

### Main functional groups (file roles, not an internal API boundary)
- **Term representation / VM-wide headers**: `erl_term.h` (+ `erl_term.c`, `erl_term_hashing.c`), `global.h` (master internal header), `erl_vm.h` (heap/stack/register/reds constants), `sys.h`, `version.h`, `error.h`, `erl_utils.h`, `erl_threads.h`.
- **BIFs**: `bif.c`/`bif.h` (BIF helpers, error/system-limit handling) and `erl_bif_*.c` — `atomics`, `binary`, `chksum`, `counters`, `coverage`, `ddll`, `guard`, `info`, `lists`, `op`, `os`, `persistent`, `port`, `re` (PCRE), `trace`, `unique`; arithmetic in `erl_arith.c`, `erl_math.c`, bignum ops in `big.c`.
- **GC**: `erl_gc.c`/`erl_gc.h`, `copy.c` (term copying / message copying), `erl_global_literals.c` (shared literal area).
- **Processes / scheduling**: `erl_process.c`/`erl_process.h` (the largest single unit: process struct, scheduling, signals, priority), `erl_nfunc_sched.c` (native-function trap wrappers), `erl_sched_spec_pre_alloc.c` (scheduler-specific pre-allocated pools), `erl_proc_sig_queue.c`, `erl_monitor_link.c`, `erl_process_dict.c`, `erl_process_dump.c`, `erl_process_lock.c`, `erl_message.c`.
- **PTab (pid/port table)**: `erl_ptab.c`/`erl_ptab.h`.
- **Allocators**: `erl_alloc.c`/`erl_alloc.h` (`erts_alloc()`, type→strategy table, per-type instrumentation), `erl_alloc_util.c|.h` (carrier-based multi-block allocator framework), concrete strategies `erl_goodfit_alloc.c`, `erl_bestfit_alloc.c`, `erl_afit_alloc.c`, `erl_ao_firstfit_alloc.c`; `erl_sys_driver.h` bridges to the OS/driver allocator.
- **ETS / term storage**: `erl_db.c` (API + ycf trap loop), `erl_db_hash.c`, `erl_db_tree.c`, `erl_db_catree.c`, `erl_db_util.c`, plus headers `erl_db.h`, `erl_db_hash.h`, `erl_db_tree.h`, `erl_db_tree_util.h`, `erl_db_util.h`, `erl_db_catree.h`.
- **Binaries / bits / iolists**: `binary.c` + `erl_binary.h` (ref-counted `Binary`/`ErlDrvBinary`, magic refs, writability), `erl_bits.c|.h` (bit syntax), `erl_iolist.c|.h` (iolist→binary), `erl_bif_binary.c` (binary BIFs).
- **Atoms**: `atom.c`/`atom.h` (+ `atom.names`).
- **Code loading & loader**: `beam_load.c`, `beam_file.c` (`.beam` file reader/validator), `beam_common.c`, `beam_bif_load.c` (`erlang:load_module` etc.), `code_ix.c|.h` (2/3-generation code index scheme), `beam_ranges.c` (faulting/line tables), `beam_transform_helpers.c`, `beam_types.c` (type/beam_types representation), `beam_bp.c|.h` (breakpoints/instrumentation), `beam_catches.c` (catch/exception table), `beam_code.h`, `module.c`, `export.c`, `index.c`, `erl_fun.c` (funs), `erl_record.c` (native records), `erl_code_staged.h` (X-macro template included with `ERTS_CODE_STAGED_*` macros defined to instantiate the per-code-index "staged" tables/blobs; used by `export.c`, `erl_fun.c`, `erl_record.c`).
- **Distribution / node tables**: `dist.c`/`dist.h` (external term distribution protocol, DFLAGs), `erl_node_tables.c` (ref-counted node table), `erl_node_container_utils.h` (pid/port/ref ↔ node-table element macros), `erl_etp.c` (constants exported for the debugger), `erl_async.c`, `external.c` (external term format), `packet_parser.c` (packet framing).
- **Tracing / debugging / instrumentation**: `erl_trace.c|.h` (tracers), `erl_bif_trace.c` (`erlang:trace*`), `erl_debug.c` (DEBUG-only print/verbose categories, `DEBUG_BAD_BYTE`), `erl_debugger.c` (`erts_debug` support), `break.c`, `erl_msacc.c` (microstate accounting), `erl_flxctr.c` (decentralized/flexible counters), `erlang_lttng.c`.
- **NIF/driver C-side support**: `erl_nif.c` (NIF loader/dispatch, `ErlNifEnv`), `erl_drv_thread.c`, `erl_dyn_lock_check.c`, `erl_internal_test.c`.
- **Threading / locking / sync**: `erl_thr_progress.c` (global "all threads passed here" barrier), `erl_thr_queue.c` (many-to-one lock-free queue), `erl_lock_check.c`, `erl_lock_count.c`, `erl_lock_flags.c`.
- **Timers / time**: `time.c` (timer wheel), `erl_time_sup.c` (Erlang time services), `erl_hl_timer.c` (high-level/BIF timers), `erl_time.h`.
- **Ports / I/O**: `erl_port.h`, `erl_port_task.c|.h` (port task queues/scheduling), `erl_io_queue.c|.h` (IO queues for file/socket ports), `io.c` (port driver I/O helpers), `erl_bif_port.c`, `erl_zlib.c|.h` (zlib 3PP wrapper).
- **Crash dumps / misc**: `erl_crash_dump.c` (writes `erl_crash.dump`), `erl_unicode.c|.h` + `erl_unicode_normalize.h`, `erl_md5.c`, `erl_printf_term.c` (term→string), `erl_errno_str.c`, `erl_cpu_topology.c` (CPU/cache topology detection for schedulers), `hash.c`, `safe_hash.c` (thread-safe hash table), `register.c`, `erl_init.c` (VM bootstrap/`main` support), `erl_main.c` is in `../sys/`, not here.
- **Wrapper headers**: `dtrace-wrapper.h`, `lttng-wrapper.h` (no-op macros when the trace backend is disabled).

## Constraints
- **Never edit the generated outputs**; edit the inputs listed above and regenerate. Generated files are `erl_bif_table.c|h`, `erl_bif_list.h`, `erl_atom_table.c|h`, `erl_guard_bifs.c`, `erl_dirty_bif_wrap.c`, `erl_alloc_types.h`, `beam_opcodes.c|h`, `beam_hot.h`, `beam_warm.h`, `beam_cold.h`, `erl_map.ycf.h`, `utils.ycf.h`, `erl_db_insert_list.ycf.h`, `erlang_dtrace.h` — none of these exist in this directory.
- **`#include "global.h"` is the convention** for essentially every translation unit here (it pulls in `sys.h`, allocators, term/process/port headers); the remaining files are deliberately low-level (`hash.c`, `safe_hash.c`, `packet_parser.c`, `erl_md5.c`, `erl_printf_term.c`, `erl_iolist.c`, `beam_file.c`, `beam_transform_helpers.c`, `erl_thr_queue.c`, `erl_lock_*.c`, `erl_zlib.c`, `erl_flxctr.c`, `erlang_lttng.c`, `erl_errno_str.c`, `erl_internal_test.c`, `erl_bif_atomics.c`, `erl_bif_counters.c`, `erl_bif_unique.c`, `erl_crash_dump.c`, `erl_sched_spec_pre_alloc.c`, `erl_dyn_lock_check.c`).
- No internal library boundary: this is one flat compilation set. Do not introduce public headers here beyond the four installed ones; treat all other headers as emulator-private.
- Code must compile in every `TYPE` flavor (`opt`, `debug`, `lcnt`, `valgrind`, `asan`, `gprof`, `frmptr`, `icount`, `gcov`) and under both `FLAVOR=jit` and `FLAVOR=emu`. Keep flavor/arch-specific code out of this directory (put it in `jit/` or `emu/`).
- Concurrency is lock-free in hot paths: changes touching deferred work must respect thread progress (`erl_thr_progress`) — see `../internal_doc/ThreadProgress.md`.

## Design Decisions
- **Tagged `Eterm` representation** (`erl_term.h`): 2-bit primary tags (`HEADER`/`LIST`/`BOXED`/`IMMED1`); `IMMED1` splits into pid/port/immed2/small-int, `IMMED2` into atom/catch/nil; boxed headers carry a 4-bit sub-tag (arityval, fun, big, float, ref, bin_ref, external pid/port/ref, heap_bits, map, record, sub_bits). Boxed data reference-counted where shared (`Binary`, node containers, funs). `ET_DEBUG` (on by default in `DEBUG` builds) swaps the type-access macros for runtime-checked variants.
- **Internal vs external node containers** (`erl_node_container_utils.h`): internal pids/ports are immediates, internal refs are boxed; external ones are heap-allocated with a pointer to a ref-counted node-table element (`erl_node_tables.c`), GC'd by ref count like refc binaries.
- **PTab** (`erl_ptab.h/.c`) unifies pids and ports in one id-allocating table with links/monitors — see `../internal_doc/PTables.md`.
- **Allocator "type" model**: all internal allocation goes through `erts_alloc(ErtsAlcType_t, ...)`; types are declared in `erl_alloc.types` and mapped to one of the carrier-based strategies. `erl_alloc_util.c` implements the shared carrier machinery (single-block carriers, multi-block carriers, super carriers, carrier migration, delayed deallocation) — see `../internal_doc/{CarrierMigration,SuperCarrier,DelayedDealloc}.md`.
- **Cooperative yielding in C**: long-running BIFs are written to trap; `erts/lib_src/yielding_c_fun` rewrites marked functions in `erl_map.c`, `utils.c`, `erl_db.c` into resumable state machines (`*.ycf.h`), which is how `maps:*`, `lists:sort/*`, and the ETS insert loops avoid blocking a scheduler. Truly blocking work goes to dirty schedulers (`erl_dirty_bif.tab`).
- **Multi-generation code loading** (`code_ix.c`, `erl_code_staged.h`): three logical copies of code (active/staging/previous) selected by an index so running processes need no locks; `erl_code_staged.h` is an X-macro template included multiple times with different `ERTS_CODE_STAGED_*` macros to synthesize the staged tables for exports/funs/records. See `../internal_doc/CodeLoading.md`.
- **Public headers are the ABI**: driver/NIF code only ever sees `erl_driver.h`/`erl_nif.h`; the emulator's internal structs are hidden behind `*_TYPES_ONLY__`/`*_TYPE_ONLY` include guards used all over these headers to let low-level files pull in just the type definitions.

## Known Issues / Gotchas
- Files here that look like they should be generated but are hand-maintained: `atom.names`, `bif.tab`, `erl_dirty_bif.tab`, `predicates.tab`, `generators.tab`, `erl_alloc.types`, `erlang_dtrace.d`.
- Conversely, `erl_atom_table.*`, `erl_bif_table.*`, `erl_bif_list.h`, `erl_guard_bifs.c`, `erl_dirty_bif_wrap.c`, `erl_alloc_types.h`, `beam_opcodes.*`, `beam_hot|warm|cold.h`, `*.ycf.h` are generated into `$(TTF_DIR)` — do not look for them here and do not add them.
- The ycf outputs come from `erl_db.c` (NOT `erl_db_util.c`): `$(TTF_DIR)/erl_db_insert_list.ycf.h` is generated from `beam/erl_db.c`.
- `erlang.erl` (the BIF module stub) lives in `../preloaded/src/`; the actual implementations are `bif.c`/`erl_bif_*.c` + `bif.tab` here.
- `beam_debug.c` is the byte-code disassembler, not the `-D` debug print facility (that is `erl_debug.c`).
- Symbol names in `erl_etp.c`, `erl_debugger.c` and `erl_assert`-adjacent debug helpers are relied upon by debugger scripts and `erts_debug`; keep them stable.
- Same-looking names exist in child dirs (`beam/emu/*.tab`, `beam/jit/*/ops.tab`) — the `beam/predicates.tab` and `beam/generators.tab` here are the shared ones.

## Notes for Agents
- Authoritative design docs, read before changing internals: `../internal_doc/GarbageCollection.md`, `DelayedDealloc.md`, `CarrierMigration.md`, `SuperCarrier.md`, `PTables.md`, `ThreadProgress.md`, `PortSignals.md`, `ProcessManagementOptimizations.md`, `CheckIO.md`, `Tracing.md`, `CodeLoading.md`, `CountingInstructions.md`, `AutomaticYieldingOfCCode.md`, `beam_makeops.md` (also `BeamAsm.md` for the JIT in `jit/`).
- Adding/removing a BIF means editing `bif.tab` (and `erl_dirty_bif.tab` for dirty BIFs), not the generated tables.
- Most files are large on purpose (e.g. `dist.c`, `erl_alloc_util.c`, `bif.c`, `erl_process.c`); navigate by section comments rather than expecting small modules.
- Building only this directory requires a configured tree and the generated headers; without `erts/$(TARGET)/config.h` and `$(TTF_DIR)` most files cannot even be parsed.

## Dependencies
- `erts/$(TARGET)/config.h` (from `../../configure`) and many `HAVE_*` macros; `#ifdef HAVE_CONFIG_H` + `#include "config.h"` is standard here.
- Generated headers in `$(TTF_DIR)` (`erl_bif_table.h`, `erl_atom_table.h`, `erl_alloc_types.h`, `beam_opcodes.h`, `erl_compile_flags.h`, `*.ycf.h`).
- `erts/lib_src` internal libraries: `erl_mmap.h`, `erl_printf`, `erl_misc_utils`, `ethread`, `erts_atomic`, and the `yielding_c_fun` code generator (Perl and ERL are build-time requirements of `../utils/*`).
- Vendored 3PP reached from here: zlib (`erl_zlib.c`), PCRE (`erl_bif_re.c`), zstd, ryu (via `../utils`/`../zstd`).

## Test Strategy
- No tests in this directory. VM tests are the 104 tracked `../test/*_SUITE.erl` suites (plus the build-generated `*_no_opt_SUITE`/`*_stripped_types_SUITE` variants) run via `make emulator_test` from `ERL_TOP` or `make test` from `../`: `beam_SUITE`, `bif_SUITE`, `gc_SUITE`, `alloc_SUITE`, `dirty_bif_SUITE`, `dirty_nif_SUITE`, `nif_SUITE`, `big_SUITE`, `binary_SUITE`, `bs_*_SUITE`, `map_SUITE`, `process_SUITE`, `signal_SUITE`, `register_SUITE`, `node_container_SUITE`, `ref_SUITE`, `native_record_SUITE`, `distribution_SUITE`, `port_SUITE`, `driver_SUITE`, `trace_SUITE`/`tracer_SUITE`/`emulator_trace.spec`, `lcnt_SUITE`, `jit_SUITE`, `system_info_SUITE`, `dump_SUITE`.
- ETS behavior is tested at the stdlib level (`../../../../lib/stdlib/test/ets_SUITE.erl`), not from `../test/`.
- C-level bugs are usually reproduced under `TYPE=debug`, `asan`, `valgrind` or `lcnt` flavors; suppression/helper files live in `../asan/` and `../valgrind/`, lock-count tests use `lcnt`.

## Routing Table
- `jit/` → JSV/JIT (BeamAsm): `beam/jit/` C++ + per-arch subdirs (`x86/`, `arm/`) with their own `ops.tab`/`predicates.tab`/`generators.tab`; child of this node, owns all `.cpp` in the emulator.
- `emu/` → interpreter (`beam/emu/`): `ops.tab`, `beam_emu.c` (generated interpreter dispatch), `emu_load.c` and the `*_instrs.tab` sources.
- `../test/` → BEAM VM test suites (`*_SUITE.erl`, spec files); sibling subtree — read-only, escalate writes to `../` (erts/emulator).
- `../utils/` → code generators invoked by `../Makefile.in` (`beam_makeops`, `make_tables`, `make_alloc_types`, `make_preload`, `make_driver_tab`, `beam_strip`, ...). Read-only from here.
- `../internal_doc/` → design documents for the VM internals (GC, carriers, PTab, thread progress, port signals, code loading, tracing, beam_makeops).
- `../nifs/`, `../drivers/`, `../sys/`, `../zlib/` → platform/3PP and port/NIF implementations that call back into functions exported from this directory (`erl_drv_nif`, `erts_atomic`, `erts_mmap`).

## See Also
- `../CONTEXT.md` — emulator build, `FLAVOR`/`TYPE` matrix, `Makefile.in` rules.
- `../../CONTEXT.md` — erts build/install layout and the `erts` application.
- `../../../erts/preloaded/src/erlang.erl` — BIF stub; `../../../erts/lib_src/yielding_c_fun/` — the YCF generator.
