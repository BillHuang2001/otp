# erts/lib_src/common/ — portable erts/ethread C support sources

## Intent
Hand-written (plus one generated) C sources for the two lowest-level static libraries in ERTS:
the `erts_internal` printf/OS-utility library and the `libethread` threading-primitive library.
These are the primitives the BEAM emulator, `epmd`, `erlexec`/`erl_child_setup` and `yielding_c_fun` are built on, and they must compile and link before anything else in the tree (in particular before any Erlang code exists in a bootstrap build).
The directory has no subdirectories, no `Makefile` and no build outputs of its own: everything is driven by `../Makefile.in`.

## API Surface
7 tracked C files, ~12.5 kLOC total (but ~4.7 kLOC of that is generated):

- `erl_printf_format.c` (1027) — the *formatting* engine. Entry point `erts_printf_format(fmtfn_t, void*, char *fmt, va_list)` plus the `erts_printf_{char,string,buf,pointer,uword,sword,uword64,sword64,double}` writers.
  Implements erts' own, OTP-extended printf grammar (documented in the file header): flags `#0- +'I`, `hh|h|l|ll|L|j|t|b<sz>` length modifiers, `d i o u x X e E f F g G c s p n T %`, plus the OTP-only `b<sz>` / `bp` / `be` integer-length forms.
  `%T` prints an `ErlPfEterm` and delegates to the global hook `erts_printf_eterm_func` (installed by the emulator from `erts/emulator/beam/erl_init.c`); without the hook `%T` returns `-EINVAL`.
  Errors are reported as negative errno-style values; callers must check the return value.
- `erl_printf.c` (488) — the *destination* layer: `erts_printf/fprintf/fdprintf/sprintf/snprintf/dsprintf/cbprintf` and the matching `erts_v*` variants.
  Provides the pluggable write callbacks `erts_write_fp`, `erts_write_fd`, `erts_write_ds` and the growable-buffer type `erts_dsprintf_buf_t` (caller supplies the `grow` callback; `ERTS_DSPRINTF_BUF_INITER`).
  Defines the emulator-settable globals declared in `erl_printf.h`: `erts_printf_stdout_func` / `erts_printf_stderr_func` (interception hooks), `erts_printf_add_cr_to_stdout` / `_stderr` (CRLF mode, set to 1 by `erts/emulator/sys/unix/sys.c`), `erts_printf_block_fpe` / `erts_printf_unblock_fpe`.
- `erl_misc_utils.c` (2122) — OS utilities behind `erts/include/internal/erl_misc_utils.h`:
  opaque `erts_cpu_info_t` created/updated/destroyed with `erts_cpu_info_create|update|destroy`; queries `erts_get_cpu_{configured,online,available,quota}`; topology (`erts_get_cpu_topology{,_size}`, `erts_cpu_topology_t` with node/processor/core/thread/logical); affinity (`erts_get_available_cpu`, `erts_is_cpu_available`, `erts_bind_to_cpu`, `erts_{unbind_from_cpu,_get_unbind_from_cpu_str,_unbind_from_cpu_str}`); cgroup CPU-quota detection; `erts_milli_sleep`; win32 `erts_map_win_error_to_errno` / `erts_get_last_win_errno`.
  It also pulls in `ethread_inline.h`, i.e. the misc-utils library depends on the ethread headers (but not on the ethread library).
- `ethr_aux.c` (763) — the non-lock core of ethread: the global ethread state (`ethr_mem__`, `ethr_pagesize__`, `ethr_min_stack_size__`/`ethr_max_stack_size__`, `ethr_not_inited__`/`ethr_not_completely_inited__`, `ethr_thr_prepare|parent|child_func__`, `ethr_runtime__`), `ethr_init_common__` / `ethr_late_init_common__` (which also calls `ethr_init_atomics`), exit handlers, the thread-specific-event pool, stack-limit handling, the `ethr_spinlock_*` / `ethr_rwlock_*` wrappers, and `ethr_fatal_error__` / `ethr_assert_failed`.
- `ethr_mutex.c` (3350) — the `ethr_mutex`, `ethr_cond` and reader-group-aware `ethr_rwmutex` implementations, plus `ethr_mutex_lib_init` / `ethr_mutex_lib_late_init` and `ethr_rwmutex_set_reader_group`.
  It contains several mutually exclusive implementations selected by platform macros set up in `ethread.h` (own spin lock implementation, `pthread_mutex`-based, win32 `CRITICAL_SECTION`-based, and a forced pure-pthread variant used under VALGRIND).
  Spin counts and the `ethr_mutex_opt` / `ethr_cond_opt` / `ethr_rwmutex_opt` tuning knobs live in `ethr_mutex.h`.
- `ethr_cbf.c` (39) — one function, `ethr_compiler_barrier_fallback()`; deliberately kept alone so the compiler cannot optimise the (intentionally empty) barrier away.
  `ETHR_COMPILER_BARRIER` maps to it in the generated `ethr_atomics.h` when no native barrier is available.
- `ethr_atomics.c` (4707) — **GENERATED, do not edit**. The native/fallback atomic + double-word-atomic implementation for the ethread API.

### Library split (defined in `../Makefile.in`)
- `ERTS_INTERNAL_LIB_SRCS` = `common/erl_printf_format.c common/erl_printf.c common/erl_misc_utils.c` → `../lib/internal/$(TARGET)/liberts_internal$(TYPE_SUFFIX).a` (+ `liberts_internal_r$(TYPE_SUFFIX).a` when `ETHR_LIB_NAME` is non-empty).
- `ETHREAD_LIB_SRC` = `common/ethr_aux.c common/ethr_atomics.c common/ethr_mutex.c common/ethr_cbf.c` **plus** the per-OS `$(ETHR_THR_LIB_BASE_DIR)/ethread.c` and `ethr_event.c` (`ETHR_THR_LIB_BASE_DIR` is `pthread` or `win`), plus `pthread/ethr_x86_sse2_asm.c` when `ETHR_X86_SSE2_ASM=yes` → `../lib/internal/$(TARGET)/libethread$(TYPE_SUFFIX).a`.
  The two halves are one library; `common/` holds the portable parts and `../pthread/` | `../win/` the OS parts.
- When `ETHR_LIB_NAME` is empty (no thread library found) the whole `ethread` library, the `_r` variants and the compilation of `common/ethr_*.c` are skipped entirely.

### Build mechanics that affect this directory
- Object dirs are `obj/$(TARGET)/$(TYPE)` (plain) and `obj/$(TARGET)/$(TYPE)/r` (reentrant) under `erts/lib_src/`; `$(TYPE)` ∈ `opt|debug|lcnt|valgrind|asan|gprof|frmptr|icount|gcov` and becomes `$(TYPE_SUFFIX)` (`.debug`, `.lcnt`, …) in the library names.
- Reentrant objects are compiled with `$(THR_DEFS)` (from the generated `erts/include/internal/$(TARGET)/ethread.mk`, derived from `ethread.mk.in` with `-D_GNU_SOURCE` de-duplicated); plain objects are not.
- Includes: `-I../include -I../include/$(TARGET) -I../include/internal -I../include/internal/$(TARGET) -I../emulator/beam -I../emulator/sys/$(ERLANG_OSTYPE)`.
- Consumers: `erts/emulator` (`DEPLIBS += erts/lib_src/obj/$(TARGET)/$(TYPE)/MADE`, links `-lerts_internal$(TYPEMARKER)` and `$(EMU_THR_LIBS)` with `-lerts_internal_r` → `-lerts_internal_r$(TYPEMARKER)`), `erts/etc/common`, `erts/epmd`.
- `../Makefile.in` `release_spec` installs the corresponding headers into `erts-$(VSN)/include/internal/` and the libraries into `erts-$(VSN)/lib/internal/`.

## Constraints
- **Never hand-edit `ethr_atomics.c`** (or the generated header `erts/include/internal/ethr_atomics.h`). Both files carry a "DO NOT EDIT / generated by `$ERL_TOP/erts/lib_src/utils/make_atomics_api`" banner.
  Regenerate from `ERL_TOP` with `erts/lib_src/utils/make_atomics_api` (an escript writing both paths); no Makefile target invokes it, so regeneration is manual and easy to forget — change the escript, not the generated output.
- These sources must stay free of dependencies on `erts/emulator/beam` internals beyond what the installed internal headers declare — they are compiled *for* the emulator, not into it.
- `erl_printf*.c` must keep working in contexts where the emulator's hooks are unset (bootstrap tools, `yielding_c_fun`, test programs).
- Linux builds require `-D_GNU_SOURCE`; Windows requires `_WIN32_WINNT >= 0x0403`. `ethread.h` enforces both with `#error`.
- Headers under `erts/include/internal/` are explicitly **not public API** (see `erts/include/internal/README`); their ABI may change without notice.

## Design Decisions
- **One format engine, pluggable sinks**: `erts_printf_format` takes a `fmtfn_t` callback, so the same parser serves `FILE *`, raw fd, fixed string, growable buffer and arbitrary caller callbacks (`erts_cbprintf`). Behaviour such as CRLF insertion is layered on top as a wrapper callback (`write_f_add_cr`).
- **stdout/stderr are written with raw `write(2)`, not stdio**, because shell drivers may put those descriptors in non-blocking mode where stdio behaves unpredictably (comment in `erl_printf.c`). `erl_errno.h` exists to paper over `ENOTSUP`/`EWOULDBLOCK`/`ETIMEDOUT`/`EINTR` portability holes.
- **Erlang-term printing is inverted through a hook** (`erts_printf_eterm_func`) so the printf library needs no knowledge of `Eterm`.
- **Atomics are generated by a code generator** so that every size class gets the whole family of barrier-annotated operations (`cmpxchg/xchg/set/add/inc/dec/read/…` × `_rb/_wb/_acqb/_relb/_ddrb/_mb`), with fallbacks (per-address spinlock table, or an "atomic modification counter" for word/double-word ops) when native ops are missing.
- **ethread is split by thread library** (`common/` portable + `pthread/`|`win/` OS-specific) rather than by `#ifdef` in one file; `ethr_internal.h` holds the internal cross-file exports.
- **`ethr_cbf.c` exists solely** to give the no-op compiler barrier a compilation unit of its own.

## Known Issues / Gotchas
- The grammar comment at the top of `erl_printf_format.c` advertises `%a`, `%A` and `%R` conversions that the implementation does **not** handle (only `%T` is implemented). Don't trust that comment.
- `ethr_internal.h` says several symbols are "implemented in lib_src/common/ethread_aux.c" — the file is actually `ethr_aux.c`. Stale comment.
- `ethread.h` declares `void ethr_compiler_barrier(void)` but no definition exists anywhere in `lib_src` (only `ethr_compiler_barrier_fallback()` in `ethr_cbf.c`). It is referenced only from the `ETHR_YIELD()` fallback path, so it normally never needs to link.
- Fallback atomics require `ethr_init_atomics()` (called from `ethr_init_common__`) to have initialised `ethr_atomic_protection__` first; using ethread atomics before `ethr_init` is a bug.
- `read_topology`, `read_cpu_quota` and `erts_map_win_error_to_errno` appear several times in `erl_misc_utils.c` under mutually exclusive platform `#ifdef`s (`__linux__`, `HAVE_KSTAT` on Solaris, `__WIN32__`, `__FreeBSD__`, plus an `#else` fallback). Grep results showing "duplicates" are not duplicates.
- `erts_get_unbind_from_cpu_str()` returns either a string literal or a pointer into the `erts_cpu_info_t` (its `affinity_str_buf`); the caller must not free it, and it is invalidated by `erts_cpu_info_update`/`destroy`.
- Capability probing convention: call `erts_bind_to_cpu` with a negative cpu id — `-EINVAL` means "supported", `-ENOTSUP` means "not supported here".
- `ERTS_PREMATURE_TIMEOUT` (`erl_misc_utils.h`) is a no-op everywhere except Darwin.
- CPU topology reading only exists for linux/kstat/win32/FreeBSD (`ERTS_CPU_TOPOLOGY_ENABLED`); on other platforms the topology functions return errors.
- `ethr_atomics.c` and `ethr_mutex.c` are huge because they contain a full implementation per platform/word-size combination; do not try to read them linearly.
- Several facilities in `ethr_mutex.c` are **never compiled** because their gate macros are never defined anywhere in the tree: `ETHR_RLOCK_WITH_INC_DEC` (its only `#define` sits inside `#if 0`), `ETHR_MTX_HARD_DEBUG_Q`/`ETHR_MTX_HARD_DEBUG_WSQ` (the former commented out; the latter gated by `ETHR_MTX_HARD_DEBUG`, itself inside `#if 0` in `ethr_mutex.h`), and `ETHR_DBG_WIN_MTX_WITH_PTHREADS` (commented out at `ethr_mutex.h:63`). Their `#else` branches are always taken, so both definitions of `hard_debug_chk_q__`, `rwmutex_incdec_restore_failed_tryrlock` and the static `InitializeCriticalSectionAndSpinCount`/`DeleteCriticalSection` overrides exist only in dead branches. `ethr_mutex.c:327-348` (`#if 0 /* Not used */`) likewise still defines the uncalled `rwmutex_freqread_rdrs_dec`.
- Defined-but-never-expanded macros (safe to remove): `ETHR_SPIN_WITH_WAITERS` (`ethr_mutex.c:40`), `FMTL_no` and `FMTF_MASK` (`erl_printf_format.c:116,133`), `ERTS_MU_RELATION_CACHE` (`erl_misc_utils.c:1494`).

## Notes for Agents
- There is nothing to build or run inside this directory: no Makefile, no tests, no outputs. Use `../Makefile.in` for build questions and `../obj/$(TARGET)/$(TYPE)/` for artefacts.
- Effective size is ~7.8 kLOC of hand-written code; `ethr_atomics.c` (4.7 kLOC) is generated and only needs reading if you touch the generator.
- Before changing scheduling/lock/atomic code, read the design notes in `../../emulator/internal_doc/` (e.g. `BeamAsm.md`, `ThreadProgress.md`, `ProcessManagementOptimizations.md`).

## Dependencies
- A C compiler, and a C++ compiler only indirectly via the emulator.
- Thread library: pthreads or win32 threads, located by `ERL_FIND_ETHR_LIB` in `../make/autoconf/otp.m4`; that macro fills `ETHR_LIB_NAME`, `ETHR_DEFS`, `ETHR_THR_LIB_BASE`, `ETHR_LIBS` in the generated `erts/include/internal/$(TARGET)/ethread.mk`.
  Diagnostics in that generated file: if it reports "No thread library found", `common/ethr_*.c` is never compiled.
- Optional `libatomic_ops` (`--with-libatomic_ops=PATH`) supplies the `libatomic_ops/` atomic backend.
- Generated `config.h` / `ethread_header_config.h` are prerequisite for compiling anything here.

## Test Strategy
- `../../../erts/test/ethread_SUITE.erl` + `ethread_SUITE_data/{ethread_tests.c,Makefile.src}`: compiles a standalone C program against `-lerts_internal_r` + `$(ETHR_LIBS)` and drives cases (threads, tids, mutex, try_lock, cond_wait, broadcast, detached, max_threads, tsd, spinlock, rwspinlock, rwmutex, atomic, dw_atomic_massage, thread_name) over a port.
- `../../../erts/test/erl_print_SUITE.erl` + `erl_print_SUITE_data/{erl_print_tests.c,Makefile.src,snprintf_test.h,integer_test.h,integer_64_test.h,string_test.h,character_test.h}`: builds two variants (`erl_print_tests.true` with `-DTHREAD_SAFE` and `.false` without) to cover both the plain and the reentrant library.
- Both `Makefile.src` templates are filled in by `lib/common_test/test_server/ts_erl_config.erl`, which supplies `@erts_lib_internal_path@` (→ `erts/lib/internal/$(TARGET)`), `@erts_lib_include_internal{,_generated}@`, `@type_marker@`, and includes `@erts_lib_make_ethread@` / `@erts_lib_make_internal@` (the generated `erts/include/internal/$(TARGET)/ethread.mk` and `erts_internal.mk`). They link `-lerts_internal[@type_marker@]` / `-lerts_internal_r[@type_marker@]`, so a *configured and built* tree — including the generated `ethread.mk` — is mandatory.
- Run them through `make test` in `erts/` (requires a full release; `erts/Makefile` sets `TEST_NEEDS_RELEASE=true`); they are not runnable standalone.

## Routing Table
No child directories — this node is a leaf. Related nodes (siblings/ancestors; read-only from here, escalate writes to the parent):
- `../` (`erts/lib_src/`) → owner of this directory: `Makefile.in` that compiles these sources, plus `utils/` and `yielding_c_fun/`
- `../pthread/`, `../win/` → the OS-specific halves of `libethread` (`ethread.c`, `ethr_event.c`, `pthread/ethr_x86_sse2_asm.c`)
- `../utils/make_atomics_api` → the generator for `ethr_atomics.c` and `erts/include/internal/ethr_atomics.h`
- `../../include/internal/` → all interfaces used here (`ethread.h`, `ethread_inline.h`, `ethr_internal.h`, `ethr_mutex.h`, `ethr_optimized_fallbacks.h`, `ethr_atomics.h`, `erl_printf.h`, `erl_printf_format.h`, `erl_misc_utils.h`, `erl_errno.h`) and the per-arch atomic backends
- `../../../erts/test/ethread_SUITE*`, `../../../erts/test/erl_print_SUITE*` → the test suites that build standalone programs against these libraries
- `../../emulator/` → primary consumer; `beam/erl_init.c` and `sys/unix/sys.c` install the `erts_printf_*` hooks

## See Also
- `../../emulator/internal_doc/` → VM internals design notes (scheduling, thread progress, ports)
- `../../../lib/common_test/test_server/ts_erl_config.erl` → how the test `Makefile.src` templates get their paths
- `../../../make/autoconf/otp.m4` (`ERL_FIND_ETHR_LIB`) → how the ethread configuration values are probed
