# erts/lib_src/pthread/ — POSIX/pthreads backend of the ethread library

## Intent
The POSIX half of the *ethread* abstraction (Erlang's portable thread/mutex/event library).
Supplies the OS-dependent primitives — thread creation + thread-specific-event (TSE) plumbing, event (wait/wake) primitives, monotonic time, thread names, signal helpers, arch runtime probes — on top of the portable half in `../common/`.
Together they are archived into the single static lib `erts/lib/internal/$(TARGET)/libethread$(TYPE_SUFFIX).a` (`.lib` with no `lib` prefix on Windows; see `LIB_PREFIX`/`LIB_SUFFIX` in `erts/lib_src/Makefile.in`), which is the only synchronization layer used by the BEAM emulator and by other OTP-internal programs linking `-lethread`.
This is an OTP-internal library (no public API stability promise); its headers are installed into `erts-$(VSN)/include/internal/`.

## Layout / Boundary
- The thread backend is chosen per-OS by directory, not by `#ifdef`s: `erts/lib_src/Makefile.in` sets `ETHR_THR_LIB_BASE_DIR=@ETHR_THR_LIB_BASE_DIR@` (`pthread` on every POSIX host, `win` on win32; picked by `ERL_FIND_ETHR_LIB` in `make/autoconf/otp.m4`, invoked from `erts/configure.ac`; that macro delegates to the lower-level `LM_CHECK_THR_LIB` helper) and compiles `$(ETHR_THR_LIB_BASE_DIR)/ethread.c` + `.../ethr_event.c` into libethread.
- This directory is therefore only HALF of libethread; the other half (`ethr_aux.c`, `ethr_atomics.c`, `ethr_mutex.c`, `ethr_cbf.c`) lives in `erts/lib_src/common/` and is OS-independent.
- Mutexes/spinlocks are NOT implemented here: `erts/include/internal/ethr_mutex.h` selects `pthread_mutex_t` for `ETHR_PTHREADS` and `../common/ethr_mutex.c` holds the out-of-line ops (including the "own rwmutex" built on `ethr_event` + TSE, since `ethread.h` defines `ETHR_USE_OWN_RWMTX_IMPL__` for pthreads).
- Per-OS headers live outside this dir: `erts/include/internal/pthread/ethr_event.h` (POSIX event struct + `ETHR_FUTEX__` + inline set/reset) vs `erts/include/internal/win/ethr_event.h`. Both are installed into `erts-$(VSN)/include/internal/<dir>/` (`INTERNAL_X_RELEASE_INCLUDE_DIRS` in `erts/lib_src/Makefile.in`).

## API Surface (the contract this backend must satisfy)
Declared in `erts/include/internal/ethread.h`, `erts/include/internal/ethr_internal.h` and `erts/include/internal/pthread/ethr_event.h`; implemented here. `erts/lib_src/win/` implements the same set — the two backends must stay symbol-compatible.
- Lifecycle: `ethr_init` / `ethr_late_init` (must call `ethr_init_common__` / `ethr_late_init_common__` from `../common/ethr_aux.c`, and maintain `ethr_not_inited__` / `ethr_not_completely_inited__`), `ethr_abort__`.
- Threads: `ethr_thr_create`, `ethr_thr_join`, `ethr_thr_detach`, `ethr_thr_exit`, `ethr_self`, `ethr_getname`, `ethr_setname`, `ethr_equal_tids`.
- Thread-specific data: `ethr_tsd_key_create`, `ethr_tsd_key_delete`, `ethr_tsd_set`, `ethr_tsd_get`.
- TSE plumbing: `ethr_set_tse__`, `ethr_get_tse__`, the exported `pthread_key_t ethr_ts_event_key__`, and the `ETHREAD_IMPL__` instantiations of `ethr_lookup_ts_event__`, `ethr_peek_ts_event`, `ethr_unpeek_ts_event`, `ethr_use_ts_event`, `ethr_get_ts_event`, `ethr_leave_ts_event`.
- Events: `ethr_init_event__`, `ethr_event_init`, `ethr_event_prepare_timed`, `ethr_event_destroy`, `ethr_event_wait/swait/twait/stwait`, plus out-of-line `ethr_event_set` / `ethr_event_reset` (their `ETHR_EVENT_IMPL__` inline bodies, named `*__`, live in `pthread/ethr_event.h`).
- Time: `ethr_get_monotonic_time` (nanoseconds, `ethr_sint64_t`) and `ethr_get_monotonic_time_is_broken`.
- Signals (only when `ETHR_HAVE_ETHR_SIG_FUNCS`): `ethr_sigmask`, `ethr_sigwait`, `ethr_kill`.
- Runtime CPU probing: `ethr_x86_cpuid__` (`ETHR_X86_RUNTIME_CONF__`) and the PPC `lwsync` probe (`ETHR_PPC_RUNTIME_CONF__`); `../common/ethr_aux.c` consumes the cpuid results to fill `ethr_runtime__.conf.{have_dw_cmpxchg,have_sse2,have_rdtscp,...}`, which the atomics code then dispatches on.
- `ethr_x86_sse2_asm.c`: out-of-line `ethr_sse2_native_su_dw_atomic_read` / `ethr_sse2_native_su_dw_atomic_set`.

## Files
- `ethread.c` (~820 lines) — pthread implementation of the ethread core: `ethr_init`/`ethr_late_init`, `thr_wrapper` + child-start handshake, stack-size/guard computation, thread create/join/detach/exit/name, TSD, TSE setters, signal wrappers, monotonic clock (clock_gettime / mach clock / `gethrtime` + brokenness detection), PPC `lwsync` probe, `ethr_x86_cpuid__`, `ethr_abort__`.
- `ethr_event.c` (~703 lines) — event implementation with two compile-time alternatives: Linux futex (`ETHR_LINUX_FUTEX_IMPL__` ⇒ `ETHR_HAVE_LINUX_FUTEX && ETHR_HAVE_32BIT_NATIVE_ATOMIC_OPS`) and pthread mutex/cond + non-blocking pipe + `select()` (`ETHR_PTHREADS`); `#error No ethread event implementation` otherwise. Contains `ethr_init_event__`, init/destroy/prepare_timed and all four `wait__` wrappers.
- `ethr_x86_sse2_asm.c` (34 lines) — not real code: it only `#define ETHR_X86_SSE2_ASM_C__` and includes `ethread.h`, so the SSE2 asm bodies guarded by that macro in `erts/include/internal/i386/ethr_dw_atomic.h` are compiled in this one TU. Compiled with an extra `-msse2` (explicit rule `$(r_OBJ_DIR)/ethr_x86_sse2_asm.o` in `erts/lib_src/Makefile.in`) and only when `@ETHR_X86_SSE2_ASM@=yes` (gcc + 32-bit x86 host with sse2 asm support, see `make/autoconf/otp.m4` ≈2326); adding it to other object dirs requires adding an equivalent `-msse2` rule, and on non-x86 or without configure support it is not built at all. Rationale: `movq` load/store via an xmm register is one atomic 64-bit access on 32-bit x86 (much faster than `cmpxchg8b`), but `-msse2` must not be applied to the whole system; the runtime check `ETHR_X86_RUNTIME_CONF_HAVE_SSE2__` falls back to `cmpxchg8b` on pre-SSE2 CPUs so the binary stays portable.

## Constraints
- Must be compiled with `$(THR_DEFS)` (= `$(ETHR_DEFS)`), not plain CFLAGS: on Linux `ethread.h` `#error`s without `-D_GNU_SOURCE`, and the POSIX feature macros (`-D_THREAD_SAFE -D_REENTRANT -DPOSIX_THREADS ...`) come from configure. `erts/lib_src/Makefile.in` filters a duplicate `-D_GNU_SOURCE` out of CFLAGS before adding `THR_DEFS`.
- A configured tree is mandatory: `config.h` (defines `ETHR_HAVE_ETHREAD_DEFINES` and all `ETHR_HAVE_*`/`ETHR_*` feature macros), plus generated `erts/include/internal/$(TARGET)/ethread_header_config.h` and `erts/include/internal/$(TARGET)/ethread.mk`. Include path used by the build: `-I../include -I../include/$(TARGET) -I../include/internal -I../include/internal/$(TARGET)`.
- `ethread.c` `#error`s if `ETHR_HAVE_ETHREAD_DEFINES` is not visible → `config.h` must be included first (both .c files do this under `HAVE_CONFIG_H`).
- On POSIX the libethread objects are taken from the *reentrant* object dir `obj/$(TARGET)/$(TYPE)/r/` only (`ETHREAD_LIB_OBJS = $(r_OBJ_DIR)/...` for non-MSVC); do not expect a plain/`r`-less copy of these translation units in the archive.
- Symbol naming convention: `#define ETHR_INLINE_FUNC_NAME_(X) X ## __` at the top of both .c files makes the backend headers' inline bodies instantiate as `foo__` (static), while the plain names are the out-of-line exports defined at the bottom of the .c file. Do not "clean up" this renaming.
- Relative `#include "ethr_dw_atomic.h"` / `"ethread.h"` resolve against the including file's arch dir (`i386/`, `gcc/`, `win/`, `libatomic_ops/`); the arch header is picked by macros in `erts/include/internal/ethread.h`.
- No public ABI/API guarantees: everything here is OTP-internal (`ETHR_*`, `ERTS_*`, never `erts_*` public headers).

## Design Decisions
- **Per-OS directory over `#ifdef`**: the backend files contain only code valid for the chosen thread library, so the pthread/win implementations stay readable and their (short) macro guards are meaningful (e.g. futex vs condvar, Darwin fdsets, Haiku).
- **Child-start handshake**: `ethr_thr_create` copies parameters into a stack-allocated `ethr_thr_wrap_data__`, the child builds its own TSE + runs `ethr_thr_child_func__` (calls `ethr_set_stacklimit__`) and publishes the result in an atomic; the parent spins `child_wait_spin_count` times then waits on *its own* `ts_event` with an event wait. `child_wait_spin_count` is `ETHR_CHILD_WAIT_SPIN_COUNT` (4000) but set to 0 when only one CPU is configured (`erts_get_cpu_configured`) — spinning only pays off on SMP.
- **Event state machine** (`pthread/ethr_event.h`): `ON=0`, `OFF=1`, `OFF_WAITER=-1` (blocked in cond) and `OFF_WAITER_SELECT=-2` (blocked in `select()`); `ETHR_EVENT_IS_WAITING__(v) = (v) < 0`. The third state exists because a waiter blocked in `select()` must be woken by `write()` on the pipe, not by `pthread_cond_signal`; `ethr_event_set__` branches on it and asserts `fd >= 0`.
- **Lazy pipe / `prepare_timed`**: an event starts either in "cond-timedwait with monotonic clock" mode (`fd = -2`) or with no pipe (`fd = -1`); the pipe is created on the first timed wait (`ethr_event_prepare_timed`) or lazily inside `wait__`. Callers that may need a timed wait on a TSE event call `erts_tse_prepare_timed` (emulator wrapper in `erts/emulator/beam/erl_threads.h`).
- **Monotonic clock chosen at `ethr_init`**: `init_get_monotonic_time()` is called BEFORE `ethr_init_common__()` (which calls `ethr_init_event__()`), because the pthread event impl only sets up a `pthread_condattr_t` with `ETHR_MONOTONIC_CLOCK_ID` if `ethr_get_monotonic_time_is_broken()` is false. Brokenness is detected from `uname` (Linux < 2.6.33; SunOS < 5.8 with >1 CPU) — such hosts silently use the pipe/`select` path and `ethr_get_monotonic_time()` returns 0.
- **Huge timeouts**: `select()` is clamped to `ETHR_SELECT_MAX_TV_SEC__` (31 days, the POSIX minimum guarantee) and `cond_timeout.tv_sec` to `INT_MAX`; on such a clamp the wait returns `EINTR` (spurious wakeup) instead of `ETIMEDOUT`, so the caller must restart the wait. With `ERTS_USE_PREMATURE_TIMEOUT` (from `erl_misc_utils.h`) the waits additionally subtract a bounded "premature" amount and re-verify the timeout.
- **Stack sizing**: `suggested_stack_size` is in kilo words; it is converted with `ETHR_KW2B`, page-aligned, plus one page and (if `ETHR_HAVE_PTHREAD_ATTR_SETGUARDSIZE`) `ETHR_STACK_GUARD_SIZE`, clamped to `ethr_min_stack_size__`/`ethr_max_stack_size__`; in `ETHR_DEBUG` it is halved for margin; `ETHR_MODIFIED_DEFAULT_STACK_SIZE` overrides the system default.
- **Thread scope**: `pthread_attr_setscope(PTHREAD_SCOPE_SYSTEM)` is attempted and `ENOTSUP` is tolerated.
- **`ethr_event.c` includes `erl_misc_utils.h`** solely for the premature-timeout macros (`ERTS_USE_PREMATURE_TIMEOUT` / `ERTS_PREMATURE_TIMEOUT()`); the rest of this dir does not call erts-level APIs.
- The emulator consumes this code indirectly: `erts/emulator/beam/erl_threads.h` wraps TSE/event calls as `erts_tse_*` (`erl_process.c`, `erl_async.c`, `erl_process_lock.c`, `erl_thr_progress.c`, `erl_nif.c`, `erl_proc_sig_queue.c`).

## Known Issues / Gotchas
- `select()`-based waiting is limited by `FD_SETSIZE`: `setup_nonblocking_pipe` calls `ETHR_FATAL_ERROR__(ENOTSUP)` when `e->fd[0] >= FD_SETSIZE` on every platform except Darwin, which instead heap-allocates `ethr_event_fdsets__` (freed in destroy) to lift the limit. Underlying cause is POSIX.1-2001 `select()`; the fix is the Darwin-style dynamic fdset or avoiding the pipe.
- Sentinel FDs: `-1` = no pipe yet, `-2` = cond-timedwait mode; `ethr_event_destroy` only closes `e->fd[...]` when `fd[0] >= 0`, so don't add pipe handling that assumes valid fds for all events.
- `thr_wrapper` must not touch `twd` after storing `twd->result` (the parent may already have returned and destroyed its stack frame) — the code comments this explicitly.
- `ethr_thr_create` returns `EINVAL` for an options name with `strlen >= 16`, whereas `ethr_setname` silently ignores names longer than `ETHR_THR_NAME_MAX` (15); long names are never truncated.
- `ethr_init` is one-shot and not thread-safe: a second call returns `EINVAL` (and leaves `ethr_not_inited__` set on failure). `ETHR_XCHK` preconditions (`EACCES` before init) disappear in non-debug builds.
- Event waits return `ETIMEDOUT`, `EINTR` (spurious/lingering writer — must be retried by the caller) or `0`; callers must not treat `EINTR` as "event set".
- `ethr_event_reset` before the first `set` is the documented pattern (used by `ethr_thr_create`'s parent loop); a `set` that races with the pipe cleanup in `wait__` is handled by re-checking state after draining — do not reorder that check.
- `erts/include/internal/ethr_internal.h:81` says "implemented in `lib_src/common/ethread_aux.c`" — stale name; the actual file is `erts/lib_src/common/ethr_aux.c` (line 55 of the same header, pointing at `lib_src/<thr-lib>/ethread.c`, is accurate).
- `ETHR_HAVE_PTHREAD_COND_TIMEDWAIT_MONOTONIC` is `#undef`ed in `ethr_event.c` when `ETHR_HAVE_ETHR_GET_MONOTONIC_TIME` or `ETHR_MONOTONIC_CLOCK_ID` is missing — the header's `monotonic_clock_cond_attr_p` path is only meaningful when all three line up.
- Thread-name support is a zoo of `pthread_setname_np`/`pthread_set_name_np`/Haiku variants selected by configure macros; `ethr_getname` returns `ENOSYS` on hosts without a 2- or 3-arg `pthread_getname_np`.
- `ethr_sigmask` uses `sigprocmask` on Android instead of `pthread_sigmask`.

## Notes for Agents
- Without configuring the tree you cannot compile or even fully preprocess these files; `config.h` and `ethread_header_config.h` are generated.
- Feature detection for everything here lives in `make/autoconf/otp.m4` (the `ETHR_*` sections; `ETHR_X86_SSE2_ASM`, `ETHR_HAVE_LINUX_FUTEX`, `ETHR_HAVE_PTHREAD_COND_TIMEDWAIT_MONOTONIC`, `pthread_setname_np` variants, `ETHR_MODIFIED_DEFAULT_STACK_SIZE`) — edit the `.m4`/`configure.ac` and regenerate, never the generated `configure`.
- Files in this dir are hand-written and small; there are no generated sources here.
- A symbol-level audit of the three `.c` files (every `static` function/object, every `typedef`, every `#define`, every `#if 0` and every guard macro) finds **no dead code**: each static is referenced inside its own file, each locally defined macro is consumed by `../../include/internal/{ethread.h,pthread/ethr_event.h,i386/ethr_dw_atomic.h}`, there are no `#if 0` blocks and every guard macro is defined somewhere in the tree.
- `ethr_compiler_barrier` (`../../include/internal/ethread.h:553`) is declared but defined nowhere in `lib_src`; this dir references it only indirectly, through the `ETHR_YIELD()` fallback branch at `ethr_event.c:131` and `:431`, which is compiled only when neither `ETHR_HAVE_SCHED_YIELD` nor `ETHR_HAVE_PTHREAD_YIELD` is configured.
- `git log -p -- erts/lib_src/pthread/` is the change history (thread-name and huge-timeout work is recent).

## Dependencies
- POSIX threads, headers `pthread.h`, `unistd.h`, `fcntl.h`, `sys/select.h`, `sys/uio.h`, `signal.h`, `time.h`/`sys/time.h`, `sys/utsname.h`.
- Linux fast path: `sys/syscall.h`, `linux/futex.h`, `sys/time.h` and a futex-capable kernel (plus `ETHR_HAVE_32BIT_NATIVE_ATOMIC_OPS`).
- Monotonic clock source: `clock_gettime(CLOCK_MONOTONIC)` (may need `-lrt`), Darwin `mach/clock.h`+`mach/mach.h`, Solaris `gethrtime`; `ETHR_MONOTONIC_CLOCK_ID` comes from configure.
- Build: gcc/clang, `-pthread` via `THR_DEFS`, GNU make; `-msse2` for `ethr_x86_sse2_asm.c` on 32-bit x86 only.
- Platform oddities handled in-tree: `__HAIKU__` (`os/kernel/OS.h`), `__ANDROID__`, `__DARWIN__` (incl. `_DARWIN_UNLIMITED_SELECT` and a custom `ethr_event_fdsets___`).

## Test Strategy
- `erts/test/ethread_SUITE.erl` + `erts/test/ethread_SUITE_data/` (`ethread_tests.c`, `Makefile.src`): builds `ethread_tests` against the built library (`-lerts_internal_r $(ETHR_LIBS)`, `-I@erts_lib_include_internal@`) and runs one case per invocation, printing `ETHR-TEST-SUCCESS/SKIP/FAILURE` markers parsed by the suite.
- Cases: `create_join_thread`, `equal_tids`, `mutex`, `try_lock_mutex`, `cond_wait`, `broadcast`, `detached_thread`, `max_threads`, `tsd`, `spinlock`, `rwspinlock`, `rwmutex`, `atomic`, `dw_atomic_massage`, `thread_name`. The program skips everything (`ethr_init`-less) when compiled with `ETHR_NO_THREAD_LIB`.
- Coverage gaps: no direct case for the `ethr_event_*` / `erts_tse_*` layer (its behavior is exercised only via the emulator's own suites), none for the huge-timeout clamping, `EINTR`/spurious-wakeup paths, `FD_SETSIZE` limit, or the `select`/cond/futex mode selection. The `dw_atomic_massage` case is what reaches `ethr_x86_sse2_asm.c` on 32-bit x86 builds.
- Building/testing with `TYPE=debug` and `TYPE=valgrind` is the practical way to hit the assertions and races in this code (the emulator links the same library).

## See Also
- `../common/` → portable half of libethread: `ethr_aux.c` (TSE lifecycle, `ethr_init_common__`, `ethr_runtime__` CPU probing, allocator), `ethr_mutex.c`, `ethr_atomics.c`, `ethr_cbf.c` — same node tree, writable within `erts/lib_src`.
- `../win/` → the win32 sibling backend implementing the same symbol set (read-only from here; escalate changes to `erts/lib_src`).
- `erts/include/internal/ethread.h`, `ethr_internal.h`, `ethread_inline.h`, `ethr_mutex.h`, `pthread/ethr_event.h` → the interface this backend satisfies (owned by the `erts/include/internal/` node).
- `erts/emulator/beam/erl_threads.h` + `erts/emulator/beam/{erl_process.c,erl_async.c,erl_process_lock.c,erl_thr_progress.c,erl_nif.c,erl_proc_sig_queue.c}` → main in-tree consumers (`erts_tse_*`).
- `erts/test/ethread_SUITE.erl`, `erts/test/ethread_SUITE_data/` → test suite for libethread (sibling — read-only, escalate writes to `erts/test`).
- `make/autoconf/otp.m4` → `ERL_FIND_ETHR_LIB` (the `ETHR_*` feature detection that configures this backend; called from `erts/configure.ac`, uses the helper `LM_CHECK_THR_LIB`).

## Routing Table
- No child directories — this node is a leaf. Work on the pthread backend is done by editing `ethread.c`, `ethr_event.c`, `ethr_x86_sse2_asm.c` here (the matching public half of any new symbol belongs to `erts/include/internal/pthread/ethr_event.h`).
- `../common/` → portable ethread/misc/printf sources (sibling — escalate writes to `erts/lib_src`).
- `../win/` → win32 backend, same contract (sibling — read-only).
