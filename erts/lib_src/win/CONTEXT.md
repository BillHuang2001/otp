# erts/lib_src/win/ — ethread backend for native Win32 threads

## Intent
The Windows (Win32/Win64, MSVC and mingw) backend of the **ethread** thread abstraction library (`libethread`).
It implements the thread/event primitives that have no portable equivalent, directly on the Win32 API
(`_beginthreadex`, `TlsAlloc`/`TlsGetValue`, `CRITICAL_SECTION`, `WaitForSingleObject`, the MSVC `_Interlocked*`
intrinsics) instead of on POSIX pthreads.
`win/` is the counterpart of `../pthread/` and is selected by configure: `ERL_FIND_ETHR_LIB` in
`../../make/autoconf/otp.m4` sets `ETHR_THR_LIB_BASE_DIR=win`, `ETHR_LIB_NAME=ethread`, `THR_LIB_NAME=win32_threads`
and defines `ETHR_WIN32_THREADS` + `ETHR_HAVE_ETHREAD_DEFINES` in the generated headers.
It is **not** a standalone library: this directory holds only the platform half of `libethread`.

## API Surface — what this directory provides
Tracked files: `ethread.c` (threads, TLS, TSD, tid/join, internal exports) and `ethr_event.c` (wait/notify events).

- `ethread.c` implements the non-portable half of the public `ethread.h` API on Windows:
  `ethr_init`, `ethr_late_init`, `ethr_thr_create`, `ethr_thr_join`, `ethr_thr_detach`, `ethr_thr_exit`, `ethr_self`,
  `ethr_getname`, `ethr_setname`, `ethr_equal_tids`, `ethr_tsd_key_create`, `ethr_tsd_key_delete`, `ethr_tsd_set`,
  `ethr_tsd_get`, and the thread-specific-event accessors `ethr_lookup_ts_event__`, `ethr_peek_ts_event`,
  `ethr_unpeek_ts_event`, `ethr_use_ts_event`, `ethr_get_ts_event`, `ethr_leave_ts_event`.
- It also provides the `__`-suffixed internals that the portable half calls back into
  (declared in `../../include/internal/ethr_internal.h` and `../../include/internal/ethread.h`):
  `ethr_win_get_errno__`, `ethr_set_tse__`, `ethr_get_tse__`, `ethr_abort__`, and `ethr_x86_cpuid__`
  (only when `ETHR_X86_RUNTIME_CONF__`).
- `ethr_event.c` implements `ethr_init_event__`, `ethr_event_init`, `ethr_event_prepare_timed`,
  `ethr_event_destroy`, `ethr_event_wait`, `ethr_event_swait`, `ethr_event_twait`, `ethr_event_stwait`, plus the
  out-of-line definitions of `ethr_event_set`/`ethr_event_reset` that forward to the inline
  `ethr_event_set__`/`ethr_event_reset__` defined in `../../include/internal/win/ethr_event.h`.
- **The authoritative contract** is the hand-written header set in `../../include/internal/win/`
  (`ethread.h`, `ethr_event.h`, `ethr_atomic.h`, `ethr_dw_atomic.h`, `ethr_membar.h`) plus the generic
  `../../include/internal/ethread.h`, `ethr_internal.h`, `ethr_mutex.h`, `ethr_atomics.h`.
  Those headers are installed into `erts-$(VSN)/include/internal/win/` by `release_spec`; edit the headers there,
  not in this directory.

## Interface contract with `../common/`
- `ethread.c` + `ethr_event.c` (here) and `../common/ethr_aux.c`, `ethr_atomics.c`, `ethr_mutex.c`, `ethr_cbf.c`
  are compiled **together** into the single static archive `libethread$(TYPE_SUFFIX)` in `../lib/internal/$(TARGET)/`
  (see `../Makefile.in`, which uses `$(ETHR_THR_LIB_BASE_DIR)/ethread.c` — the file list here is not hard-coded).
  On Windows `USING_VC=yes` makes that `libethread$(TYPE_SUFFIX).lib` (`LIB_PREFIX=` empty, `LIB_SUFFIX=.lib`).
- Callback direction: `common/` owns `ethr_init_common__`, `ethr_late_init_common__`, `ethr_run_exit_handlers__`,
  `ethr_ts_event_destructor__`, `ethr_make_ts_event__`, `ethr_free_ts_event__`, `ethr_set_stacklimit__`,
  `ethr_cpu_info__`, `ethr_mem__`, `ethr_not_inited__`/`ethr_not_completely_inited__`, spinlock/rwmutex,
  atomics and mutex code. This directory owns everything that must touch the Win32 API.
- If a symbol here is renamed/removed, the matching declaration in `ethr_internal.h`/`ethread.h` and the callers in
  `../common/ethr_aux.c` / `ethr_mutex.c` must change in the same commit.

## Constraints
- Windows-only. Uses `windows.h`, `process.h`, `intrin.h`, `_beginthreadex`/`_endthreadex`, `TlsAlloc`; it cannot be
  compiled on POSIX. Conversely, do **not** add `#ifdef _WIN32` blocks to `../common/` — per-OS behaviour belongs in
  this per-OS directory (same policy as `../pthread/` and `../../emulator/{drivers,nifs,sys}/{common,unix,win32}`).
- Requires configure-defined macros: both files `#error` out if `ETHR_HAVE_ETHREAD_DEFINES` is not set, and
  `../../include/internal/ethread.h` `#error`s unless `_WIN32_WINNT >= 0x0403` was defined on the command line.
- Both files must be compiled with `#define ETHR_INLINE_FUNC_NAME_(X) X ## __` and `#define ETHREAD_IMPL__`
  (resp. `ETHR_EVENT_IMPL__`) **before** including `ethread.h`/`ethr_internal.h`; that is the mechanism that makes the
  `static inline` functions emit out-of-line `ethr_*__` symbols for the non-inlined (`ETHR_DEBUG`/`ETHR_TRY_INLINE_FUNCS`)
  build. Removing those defines breaks linking.
- `THR_DEFS` (from the generated `../../include/internal/$(TARGET)/ethread.mk`) matters more here than on unix,
  because `../Makefile.in` has no `-l`/`-L` style thread flags to fall back on for win32.
- Never edit generated files: `../../include/internal/ethr_atomics.h` is produced by
  `../utils/make_atomics_api` (header says DO NOT EDIT); edit the script and regenerate.

## Design Decisions
- **Separate directory, not `#ifdef`s** — the same per-OS-directory policy used everywhere in ERTS: the shared
  behaviour lives in `../common/`, the OS-specific thread/event/TLS primitives live here, and the compiler picks
  one backend via `ETHR_THR_LIB_BASE_DIR`.
- **Threads are created with the CRT, not the Win32 API**: `_beginthreadex`/`_endthreadex` (not `CreateThread`)
  so per-thread CRT state is initialized/torn down — required for both `/MT` and `/MD` runtimes.
- **Events are real Win32 auto-reset events, not condition variables**: `ethr_event` is
  `{ ethr_atomic32_t state; HANDLE handle; }` with tri-state `ETHR_EVENT_ON__ (0)`, `ETHR_EVENT_OFF__ (1)`,
  `ETHR_EVENT_OFF_WAITER__ (-1)`. A waiter that fails to observe ON publishes `OFF_WAITER__` and blocks on the
  handle; the setter does a full-barrier `xchg` and calls `SetEvent` only when it observes `OFF_WAITER__`, so a
  wakeup racing with a blocked waiter is never lost. Timed waits map directly onto `WaitForSingleObject`'s timeout
  (hence `ethr_event_prepare_timed` is a no-op here, unlike the pthread/futex backend).
- **Thread ids are an ethread-internal abstraction**: `ethr_tid` is `{ long id; struct ethr_join_data_ *jdata; }`,
  where `id` is an atomically incremented counter (never `ETHR_INVALID_TID_ID`, i.e. `-1`) and `jdata` holds the
  native `HANDLE` plus the thread's return value. `ethread.h` fixes `ethr_tsd_key` to `DWORD` and
  `ETHR_YIELD()` to `Sleep(0)` on `ETHR_WIN32_THREADS`.
- **TLS instead of pthread keys**: the ethread TLS slot for the thread-specific event is a `DWORD` index
  (`ethr_ts_event_key__` from `TlsAlloc`), and the per-thread `ethr_tid` is kept in a second private TLS slot
  (`own_tid_key`), both accessed with `TlsGetValue`/`TlsSetValue`. The main thread is pre-registered with id 0 in
  `ethr_init`.
- **`ethr_abort__` is `DebugBreak()`** (not `abort()`), so a fatal ethread error traps into the debugger on Windows.
- **The reentrant (`_r`) variant is expressed via the MSVC C runtime, not a `_r` object dir**: on unix
  `../Makefile.in` builds an extra `obj/.../r` tree producing `libethread_r`/`liberts_internal_r` for the reentrant
  thread library; with `USING_VC=yes` the same distinction is carried by the object sub-directories
  `obj/$(TARGET)/$(TYPE)/{MD,MDd}` (`/MD`, `/MDd` = reentrant DLL CRT) vs `{MT,MTd}` (`/MT`, `/MTd` = static CRT),
  and archive flags switch to `-out:` with `.lib` naming.
- **Windows has no signal API and no thread names**: `ETHR_HAVE_ETHR_SIG_FUNCS` is undefined, so `ethr_sigmask`/
  `ethr_sigwait`/`ethr_kill` do not exist; `ethr_getname` returns `ENOSYS` and `ethr_setname` is a no-op
  (the pthread backend implements both).
- **All failures are reported as POSIX errno**, not Win32 error codes: `ethr_win_get_errno__` →
  `erts_get_last_win_errno()` → `GetLastError()` mapped through `erts_map_win_error_to_errno()` in
  `../common/erl_misc_utils.c`. Callers in `../common/` can therefore test for `EINVAL`, `EINTR`, `ETIMEDOUT`, ….

## Known Issues / Gotchas
- **Timed waits are capped at one week**: `WaitForSingleObject` takes a `DWORD` millisecond timeout, so a larger
  nanosecond timeout is clamped to `MILLISECONDS_PER_WEEK__`; the wait then returns `EINTR` (not `ETIMEDOUT`) so the
  caller loops and re-waits. `timeout == 0` is handled without touching the kernel (pure state check).
- `ethr_thr_join`/`ethr_thr_detach` `free()` `tid.jdata`; joining or detaching the same `ethr_tid` twice is
  undefined behaviour (explicitly documented as "users responsibility" in the code).
- Threads not created by `ethr_thr_create` (native Win32 threads) are not registered: `ethr_self()` returns an
  invalid tid for them, and calling `ethr_thr_exit` from such a thread asserts and then only calls `_endthreadex(0)`.
- `child_wait_spin_count` is forced to 0 unless `erts_get_cpu_configured()` reports more than one CPU — a
  single-CPU machine spins zero times in `ethr_thr_create`'s "wait for child to initialize" loop.
- `ETHR_XCHK` (on in `ETHR_DEBUG`/`TYPE=debug` builds) turns the `ethr_not_inited__`/argument checks into real
  assertions; in optimized builds many of these checks compile away entirely.

## Notes for Agents
- Keep this file and `../pthread/ethread.c` + `../pthread/ethr_event.c` in sync whenever the portable
  `../common/ethr_*.c` API or `ethr_internal.h` changes — they are two implementations of one contract.
- The sibling `Makefile` in `../` is generated by configure; `../Makefile.in` is the source of truth for how these
  two files are compiled (object dirs, `THR_DEFS`, archive naming).
- Nothing here is exercised on a unix build: changes are only compile-tested by the Windows CI job
  (`.github/workflows/`), so review them carefully.
- Empty/ignored build outputs nearby that are not code: `../yielding_c_fun/bin/win32/` (git-ignored),
  `../lib/internal/$(TARGET)/` (receives `libethread*.lib`).

## Dependencies
- Windows toolchain: MSVC (`_MSC_VER >= 1400` required for the intrinsic-based atomics/memory barriers) or mingw,
  plus the Windows SDK headers. The atomics/membar path additionally needs `_InterlockedCompareExchange`,
  `_InterlockedExchange`, and `_InterlockedCompareExchange64`/`_InterlockedCompareExchange128` for the 64-bit
  and double-word paths.
- No external thread library is linked: `THR_LIB_NAME=win32_threads` is satisfied by the OS (`kernel32`).
- Configure-provided macros in `../../include/internal/$(TARGET)/{ethread_header_config.h,ethread.mk}` and the
  generated `../../include/internal/ethr_atomics.h` are hard inputs.
- Runtime dependency on `erts_get_last_win_errno()`/`erts_map_win_error_to_errno()` from
  `../common/erl_misc_utils.c` (part of `liberts_internal`).

## Test Strategy
- There is **no test suite in this directory**. The backend is tested indirectly on Windows by everything that
  links `libethread`: `../../emulator/test/` (thread/event/allocator/timer suites, e.g. the `*_SUITE.erl` covering
  `erts_thread`/`ethread`), `../../test/`, and `../../emulator` itself.
- Run the emulator test suites with a `TYPE=debug` build on Windows to enable `ETHR_XCHK` assertions
  (`ETHR_ASSERT`, `ETHR_FATAL_ERROR__`); the same suites must be re-run on unix to guard the shared contract.
- Coverage gap: Windows-only paths (event state machine under load, huge timeouts, join/detach) are only reached on
  the Windows CI job, not on unix developer machines.

## See Also
- `../common/` → portable half of `libethread` (`ethr_aux.c`, `ethr_atomics.c`, `ethr_mutex.c`, `ethr_cbf.c`) and the `erl_*` utility libs
- `../pthread/` → POSIX/pthread (and Linux-futex) counterpart backend — the semantic reference for this directory
- `../../include/internal/win/` → Windows ethread headers (`ethread.h`, `ethr_event.h`, `ethr_atomic.h`, `ethr_dw_atomic.h`, `ethr_membar.h`)
- `../../include/internal/ethread.h`, `ethr_internal.h`, `ethr_mutex.h` → the cross-platform API and internal contract both backends implement
- `../Makefile.in` → how `win/ethread.c` + `win/ethr_event.c` are compiled and archived (`ETHR_THR_LIB_BASE_DIR`, `USING_VC`, MD/MT object dirs)
- `../../emulator/` → main consumer of `libethread` (wrapper `beam/erl_threads.h`); `../../epmd/`, `../../etc/common/` also link it
