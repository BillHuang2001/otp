# erts/include/internal — ERTS internal (non-public) C header layer

## Intent
Private C header layer of ERTS: the **ethread** thread/atomic/mutex abstraction library, the
`erl_*` runtime utility headers (printf, errno, misc utils), and the makefile fragments describing how
to compile/link against the ethread and erts-internal libraries.
`README` states verbatim that files here are *"not for public use"*, are for Erlang/OTP applications
only, and *"may be changed at any time without prior notice"*.
This directory is **header-only**: it declares the APIs and contributes only `static inline` bodies.
Nearly all non-inline implementations live in the sibling `../../lib_src/` tree
(`common/ethr_aux.c`, `ethr_mutex.c`, `ethr_atomics.c`, `ethr_cbf.c`, `erl_misc_utils.c`,
`erl_printf.c`, `erl_printf_format.c`, `pthread/`, `win/`).
Consumers: `erts/lib_src/`, `erts/emulator/` (incl. `beam/sys.h`, `erl_utils.h`, `erl_init.c`,
`erl_threads.h`, `erl_internal_test.h`), `erts/epmd/src/`, `erts/etc/common/`, `erts/test/` C data.
**No C source under `../../lib/` includes these headers** (the only `lib/` reference is
`../../lib/common_test/test_server/ts_erl_config.erl`, which reads the installed `.mk` fragments as data).

## API Surface

### 1. ethread core (thread library API — `ethread*`)
- `ethread.h` (~860 lines) — the entire ethread API and the single entry point. Types (`ethr_tid`,
  `ethr_tsd_key`, `ethr_sint*_t`, `ethr_runtime_t`), `ETHR_INLINE`/`ETHR_FORCE_INLINE`/`ETHR_ASSERT`,
  cache-line macros (`ETHR_CACHE_LINE_SIZE`, `ETHR_CACHE_LINE_ALIGN_SIZE`), `ETHR_FATAL_ERROR__`,
  thread/TSD/`ethr_ts_event` API, `ethr_rwlock_t` typedefs, plus the conditional `#include` chain that
  pulls in everything else (see Design Decisions).
- `ethread_inline.h` (~78 lines) — compiler/version detection only (`ETHR_GCC_COMPILER`,
  `ETHR_GCC_VERSION`, `ETHR_AT_LEAST_GCC_VSN__`) and the resulting `ETHR_INLINE` definitions.
  Included first by `ethread.h:37`.
- `ethr_mutex.h` (~790 lines) — `ethr_mutex`, `ethr_rwmutex`, `ethr_cond` implementation (incl. the
  "own" rwmutex used when `ETHR_USE_OWN_RWMTX_IMPL__` is set on win32/valgrind). Included **last** by
  `ethread.h:778` because it needs the atomic declarations and ts_event, which `ethread.h` defines.
- `ethr_optimized_fallbacks.h` (~233 lines) — "optimized fallbacks used when native ops are missing":
  native spinlocks via pthread spinlocks or native atomics, native rwspinlocks via native atomics.
- `ethr_atomics.h` (~9170 lines, **generated, tracked**) — the atomics API: maps the selected native
  implementation onto `ethr_atomic32_*` / `ethr_atomic_*` / `ethr_dw_atomic_*`. Included by
  `ethread.h:416`.
- `ethr_internal.h` (~92 lines) — *non-API* ethread exports for `../../lib_src/` only:
  `ethr_mem__`, `ethr_cpu_info__`, `ethr_pagesize__`, `ethr_min_stack_size__`,
  `ethr_max_stack_size__`, `ethr_not_inited__`. Includes `erl_misc_utils.h`.

### 2. `erl_*` runtime utility headers
- `erl_errno.h` — normalises `ENOTSUP`/`EWOULDBLOCK` across platforms; included by `ethread.h:38`.
- `erl_misc_utils.h` — CPU info/topology (`erts_cpu_info_t`, `erts_cpu_topology_t`), page/stack sizes,
  memory allocator hooks. Implementation: `../../lib_src/common/erl_misc_utils.c`.
- `erl_printf.h` — `erts_dsprintf_buf_t` dynamic printf buffer API and the output hooks
  (`erts_printf_stdout_func`, `erts_printf_add_cr_to_stdout`, ...). Implementation:
  `../../lib_src/common/erl_printf.c`.
- `erl_printf_format.h` — `erts_snprintf`/`erts_vsnprintf` and the Erlang-term format specifiers.
  Implementation: `../../lib_src/common/erl_printf_format.c`. Includes `erl_int_sizes_config.h`
  (from `../` — generated per `$host`).

### 3. Makefile fragments (configure templates — `*.mk.in`)
- `ethread.mk.in` → `<host>/ethread.mk`: `ETHR_LIB_NAME`, `ETHR_DEFS`, `ETHR_LIBS`, `ETHR_X_LIBS`,
  `ETHR_THR_LIB_BASE`. `include`d by `../../lib_src/Makefile.in:25` and read by
  `../../lib/common_test/test_server/ts_erl_config.erl` to build C test programs.
- `erts_internal.mk.in` → `<host>/erts_internal.mk`: `ERTS_INTERNAL_X_LIBS`. Not `include`d by any
  Makefile; its value is substituted directly as `@ERTS_INTERNAL_X_LIBS@` into
  `emulator/Makefile.in`, `epmd/src/Makefile.in`, `etc/common/Makefile.in` and
  `test/ethread_SUITE_data/Makefile.src`. It exists so external/test build systems (common_test) can
  link `liberts_internal*`.

### 4. `<host>/` generated artefacts (absent in a clean checkout)
`<host>` is the configure host triple, e.g. `x86_64-pc-linux-gnu`, `i686-pc-linux-gnu`, `win32`.
- `ethread_header_config.h` ← `ethread_header_config.h.in`
  (`AC_CONFIG_HEADERS`, `../../configure.ac:122`, alongside `erts/<host>/config.h`).
  ~88 `ETHR_*` macros: `ETHR_SIZEOF_{PTR,INT,LONG,LONG_LONG,__INT64,__INT128_T,AO_T}`, `ETHR_BIGENDIAN`,
  `ETHR_PTHREADS` / `ETHR_WIN32_THREADS`, `ETHR_HAVE_*`, `ETHR_FORCE_PTHREAD_RWLOCK`,
  `ETHR_DISABLE_NATIVE_IMPLS`, the `ETHR_PREFER_*_NATIVE_IMPLS` knobs and
  `ETHR_TRUST_GCC_ATOMIC_BUILTINS_MEMORY_BARRIERS`.
  This is the file `ethread.h:32-34` includes first, *unless* `ETHR_HAVE_ETHREAD_DEFINES` is already
  defined. Note the same `ETHR_*` set (plus extras such as `ETHR_HAVE_ETHREAD_DEFINES` and
  `ETHR_SMP_REQUIRE_NATIVE_IMPLS`) is also emitted by autoheader into `erts/<host>/config.h`
  (`../../config.h.in` has ~98 `ETHR*` `#undef`s vs ~88 here) — so grepping only this directory for an
  `ETHR_*` definition can miss it, and `../../lib_src/` sources that `#include "config.h"` first never
  reach this header.
- `ethread.mk`, `erts_internal.mk` (AC_CONFIG_FILES, `../../configure.ac:3946-3947`).

### 5. Install/export (from `../../lib_src/Makefile.in` `release_spec`, ~lines 432-481)
- `RELEASE_INCLUDES` (`<target>/erl_int_sizes_config.h`, `erl_fixed_size_int_types.h`, both from `../`)
  → `erts-$(VSN)/include` **and** `$RELEASE_PATH/usr/include`.
- `INTERNAL_RELEASE_INCLUDES` (`README`, `ethread.h`, `ethread_inline.h`, `ethr_mutex.h`,
  `ethr_optimized_fallbacks.h`, `ethr_atomics.h`, `<target>/ethread.mk`, `<target>/erts_internal.mk`,
  `<target>/ethread_header_config.h`, `erl_printf.h`, `erl_printf_format.h`, `erl_misc_utils.h`,
  `erl_errno.h`) → `erts-$(VSN)/include/internal`.
- `INTERNAL_X_RELEASE_INCLUDE_DIRS = i386 x86_64 ppc32 sparc32 sparc64 tile gcc pthread win
  libatomic_ops` → each dir's `*.h` copied to `erts-$(VSN)/include/internal/<dir>`.

## Constraints
- **Generated, never hand-edit**: `ethr_atomics.h`. Its banner says "DO NOT EDIT … generated by
  `$ERL_TOP/erts/lib_src/utils/make_atomics_api`". That escript
  (`../../lib_src/utils/make_atomics_api`) also emits the paired
  `../../lib_src/common/ethr_atomics.c`; both are tracked in git. Change the script and regenerate,
  do not patch either output.
- **Templates, never hand-edit the outputs**: `ethread_header_config.h.in`, `ethread.mk.in`,
  `erts_internal.mk.in` are the sources; `<host>/*` are configure outputs.
- **One implementation per directory, no shared header names.** Each impl dir ships its *own*
  `ethr_membar.h`, `atomic.h`, `spinlock.h`, `rwlock.h`, `ethr_dw_atomic.h` and/or `ethread.h`,
  because `#include "x.h"` resolves against the *including file's* directory first. A subdir must
  therefore reach a sibling explicitly (`x86_64/ethread.h` → `"../i386/ethread.h"`,
  `gcc/ethr_membar.h` → `"../i386/ethr_membar.h"`).
- Only `-I erts/include/internal` and `-I erts/include/internal/$(TARGET)` (plus `../include`,
  `../include/$(TARGET)`) are on the include path (`../../lib_src/Makefile.in:152-154`,
  `../../emulator/Makefile.in:882`, `../../epmd/src/Makefile.in:42-43`,
  `../../etc/common/Makefile.in:47-48`). The arch/impl directories are **not** `-I` paths.
- Installed ≠ public: much of what is installed into `erts-$(VSN)/include/internal` is still governed
  by `README`'s "not for public use" clause.
- Adding a new impl dir requires updating `INTERNAL_X_RELEASE_INCLUDE_DIRS` in
  `../../lib_src/Makefile.in` as well as the `#include` chain in `ethread.h`.

## Design Decisions
- **Why a separate non-public layer at all.** These headers leak implementation detail (raw asm,
  platform `#defines`, internal struct layouts, `ethr_*` internals). The user-facing public surface is
  `../../emulator/beam/` (`erl_driver.h`, `erl_nif.h`, `erl_drv_nif.h`, `erl_nif_api_funcs.h`) —
  those are **not** in this header tree; `../` contributes only the semi-public
  `erl_int_sizes_config.h` /
  `erl_fixed_size_int_types.h`. This directory makes the "internal only, may change without notice"
  boundary explicit and machine-enforced by directory, not by convention.
- **Implementation selected by macro-conditional `#include`, not by `-I` order.** `ethread.h:373-410`
  hard-codes the whole selection logic, so the choice is reviewable in one place and cannot drift
  with include-path ordering:
  1. skipped entirely if `ETHR_DISABLE_NATIVE_IMPLS` (`--disable-native-ethr-impls`);
  2. if `__GNUC__` and `ETHR_PREFER_GCC_NATIVE_IMPLS` (`--enable-prefer-gcc-native-ethr-impls`) →
     `gcc/ethread.h`; if `ETHR_PREFER_LIBATOMIC_OPS_NATIVE_IMPLS` (`--with-libatomic_ops=PATH`) →
     `libatomic_ops/ethread.h`;
  3. otherwise, while `ETHR_HAVE_NATIVE_ATOMIC32/64` are still unset, pick an arch dir by
     `ETHR_SIZEOF_PTR` + compiler arch macros: 32-bit → `i386/`(`__i386__`), `ppc32/`, `sparc32/`,
     `tile/`; 64-bit → `x86_64/`, `sparc64/`; then fall through to `gcc/ethread.h` (if
     `ETHR_HAVE_GCC___ATOMIC_BUILTINS`), `libatomic_ops/ethread.h`, `gcc/ethread.h`;
  4. non-GNU compilers: `libatomic_ops/ethread.h`, or `win/ethread.h` on win32.
  Consequence: on x86/x86_64 **and** ppc32/sparc32/sparc64/tile the hand-written asm impl from the arch
  dir wins by default; architectures with no dedicated dir (aarch64, arm, riscv, s390x, …) get
  `gcc/ethread.h`'s `__atomic` builtins; `--disable-smp-require-native-atomics` downgrades to the
  spinlock/mutex fallbacks in `ethr_optimized_fallbacks.h` (configure writes a warning to
  `../../CONF_INFO`).
- **Single-source out-of-line definitions.** The same headers are included a second time by the `.c`
  files in `../../lib_src/common/` with `ETHR_TRY_INLINE_FUNCS` plus `ETHR_AUX_IMPL__` /
  `ETHR_MUTEX_IMPL__` / `ETHR_ATOMIC_IMPL__` defined, so the "inline" bodies are emitted once as real
  symbols (`ethr_aux.c:36-37`, `ethr_mutex.c:33-37`, `ethr_atomics.c:182-188`). That is why this
  directory can be header-only yet expose linkable symbols — the symbols come from
  `libethread*` / `liberts_internal*`, and why `ETHR_LIBS`/`ETHR_X_LIBS` exist at all.
- **`pthread/` holds only `ethr_event.h`** because in the pthread case *only* the event API is
  platform-specific enough to need declarations here (`ethr_event_init/destroy/wait/swait/twait/stwait`,
  plus inline `ethr_event_set/reset`); the implementation is `../../lib_src/pthread/ethr_event.c` and the
  rest of the pthread ethread implementation is `../../lib_src/pthread/ethread.c`. `win/ethr_event.h` →
  `../../lib_src/win/ethr_event.c` follows the same split. Included from `ethread.h:633-636`, before
  `struct ethr_ts_event_` needs `ethr_event`.
- Naming asymmetry is intentional and useful for grepping: `ethread*` = thread-library API layer,
  `ethr_*` = lower-level primitives (atomics, memory barriers, mutexes, aux).

## Known Issues / Gotchas
- **`win32/` ≠ `win/`.** `erts/include/internal/win32/` is the git-ignored configure output dir for the
  Windows host triple (`../../../.gitignore:81`); the *tracked* `win/` directory holds the MSVC native
  atomic/membar/event headers. Do not confuse them.
- **Nothing preprocesses without configure.** A clean checkout has no `<host>/` dir, so
  `ethread_header_config.h` does not exist and `ethread.h` cannot even be preprocessed;
  `../../lib_src/Makefile.in:25` `include`s `$(TARGET)/ethread.mk` unconditionally, so `lib_src`
  cannot be built in an unconfigured tree. Other per-`$host` ignored dirs are listed in
  `../../../.gitignore:40-49` (`i686-pc-linux-gnu`, `x86_64-pc-linux-gnu`, *-darwin, *-freebsd, …).
- **Source layout ≠ release layout.** Installed `erts-<vsn>/include/internal/` has the `.mk` files and
  `ethread_header_config.h` **flattened** (no `<host>/` subdirectory), while the impl dirs are
  preserved as named subdirectories. A path written for a source tree generally will not work against
  an installed one; `ts_erl_config.erl` handles both cases explicitly.
- **`ethr_atomics.h` is huge and highly repetitive** (~9k lines / ~390 KB, 32-bit / 64-bit /
  double-word variants of the same op set). Never read or diff it wholesale — grep for a specific
  `ETHR_ATMC*` macro or inspect `../../lib_src/utils/make_atomics_api` instead.
- Several arch dirs are thin or near-legacy: `x86_64/ethread.h` (24 lines) and `sparc64/ethread.h`
  (23 lines) are pure forwarding headers to `i386/` resp. `sparc32/`; `tile/` targets Tilera
  TILE64/TILEPro (only reachable via `../../../xcomp/erl-xcomp-TileraMDE2.0-tilepro.conf`);
  `libatomic_ops/` is only selected with `--with-libatomic_ops`. Reading one of these dirs in
  isolation is misleading — the real definitions are in the dir it forwards to.
- **Defined-but-unreferenced symbols (verified zero uses tree-wide via `git grep -w`):** `ETHR_B2KW`
  (`ethr_internal.h:47`), `ETHR_WIN_MUTEX__` (`ethr_mutex.h:235`), the typedef `ethr_uint128_t`
  (`ethread.h:270`; its symmetric partner `ethr_sint128_t` is used), the debug macros
  `ETHR_MTX_CHK_EXCL_IS_NON_EXCL` and `ETHR_MTX_CHK_EXCL_SET_NON_EXCL_NO` (`ethr_mutex.h`, defined in
  all 3 conditional branches, never expanded), `ETHR_NDWA_AOVAL2VAL__`
  (`libatomic_ops/ethr_dw_atomic.h`, 3 branches; its inverse `ETHR_NDWA_VAL2AOVAL__` is used),
  `ETHR_WIN_HAVE_XCHG_ADD_ACQ` (`win/ethr_atomic.h`, defined + `#undef`ed but never tested), and 10 of
  the `ETHR_X86_RUNTIME_CONF_HAVE_*` feature macros (`ethread.h:308-331`): the `..._RDTSCP__`,
  `..._NONSTOP_TSC__`, `..._NONSTOP_TSC_S3__` positive forms and the `..._NO_{DW_CMPXCHG,CPUID,RDTSCP,
  CONSTANT_TSC,TSC_RELIABLE_TSC,NONSTOP_TSC,NONSTOP_TSC_S3}__` forms. The other family members
  (`..._SSE2__`, `..._NO_SSE2__`, `..._DW_CMPXCHG__`, `..._CPUID__`, `..._CONSTANT_TSC__`,
  `..._TSC_RELIABLE__`, `..._META`/`..._NO_META`) *are* referenced.
- **`ETHR_X86_RUNTIME_CONF_HAVE_NO_CONSTANT_TSC__` is miswired**: it expands
  `ETHR_X86_RUNTIME_CONF_HAVE_NO_META(nonstop_tsc)` (`ethread.h:315`), not `constant_tsc`. Harmless today
  only because nothing references the macro.
- **`AO_USE_PENTIUM4_INSTRS` (`libatomic_ops/ethread.h:36`) is not dead** despite appearing once
  in-tree: it is a libatomic_ops knob consumed by the *external* `atomic_ops.h`, which is not vendored.
- **All ten impl dirs remain reachable.** Every selecting condition in `ethread.h:373-410`/`632-636` is
  satisfiable by a config this tree supports (`otp.m4` `ERL_FIND_ETHR_LIB` + `LM_HARDWARE_ARCH`,
  `xcomp/*.conf`, `HOWTO/INSTALL.md`): `i386`/`x86_64`, `ppc32` (`erl-xcomp-powerpc-dso-linux-gnu.conf`),
  `tile` (`erl-xcomp-TileraMDE2.0-tilepro.conf`), `libatomic_ops` (`--with-libatomic_ops`),
  `pthread`/`win` (configure `AC_DEFINE(ETHR_PTHREADS)`/`(ETHR_WIN32_THREADS)`), `gcc` (fall-through on
  any GNU-C target with no dedicated dir, e.g. aarch64/arm; or `--enable-prefer-gcc-native-ethr-impls`).
  `sparc32`/`sparc64` have **no xcomp config** — they are reachable only via a native SPARC build
  (`otp.m4:2237` `sparc|sun4u|sparc64|sun4v`, `otp.m4:3058-3060`, `--with-sparc-memory-order`,
  `HOWTO/INSTALL.md` lists "32/64-bit SPARC V9" as an OTP-native impl). SPARC and tile have no CI
  coverage in `.github/`.
- Installed ≠ public, and the install set is *not* the directory contents: several headers here
  (`ethr_internal.h`, `ethr_optimized_fallbacks.h`'s consumers aside, the whole `gcc/`…`win/` set) are
  shipped in the release as internal includes, while `ethr_internal.h` — the most internal file of all —
  is deliberately **not** in `INTERNAL_RELEASE_INCLUDES`. Do not treat presence in
  `erts-<vsn>/include/internal` as an API commitment; `README` governs.

## Notes for Agents
- To understand the ethread API, read `ethread.h` top-to-bottom first — it *is* the map: lines 120-223
  select the thread library (pthread/win32), 225-271 the integer types, 291-371 CPU runtime config,
  373-410 the native-impl selection, 416 the atomics API, 633-636 the event header, 778 `ethr_mutex.h`.
- To find the body of a specific `ethr_*` function: the declaration is in one of the headers here, the
  implementation is in `../../lib_src/common/*.c` (or `pthread/`/`win/`), unless it is a `static inline`
  in the header itself.
- Changing an atomics operation means changing `../../lib_src/utils/make_atomics_api`, not
  `ethr_atomics.h`.
- `ETHR_HAVE_NATIVE_ATOMIC32/64` (described in `ethr_atomics.h`) is what each impl dir sets to claim the
  atomics API; a new port must set it or the chain falls through to `gcc/`.
- Platform options live in `../../../make/autoconf/otp.m4` (`ERL_FIND_ETHR_LIB`, ~line 1634 onwards) and
  `../../configure.ac`; `../../autoconf/` only mirrors master autoconf helper files.

## Dependencies
- C99 / GNU C compiler. On win32: MSVC or mingw + `-D_WIN32_WINNT>=0x0403` (enforced by an `#error` in
  `ethread.h:167-171`) and `-D_GNU_SOURCE` on Linux (`#error` at `ethread.h:125-127`).
- Optional: **libatomic_ops** headers (only for the `libatomic_ops/` impl, located via
  `--with-libatomic_ops=PATH`).
- pthread spinlocks / `sched_yield` / monotonic-clock library when the corresponding `ETHR_HAVE_*`
  probes succeed (`make/autoconf/otp.m4`), `-lrt` on some Solaris/Linux setups.
- Link-time: `libethread*` and `liberts_internal*` from `../../lib/internal/` — see `ETHR_LIBS`,
  `ETHR_X_LIBS`, `ERTS_INTERNAL_X_LIBS`.
- A configured tree is mandatory for anything to preprocess (`<host>/ethread_header_config.h`).

## Test Strategy
- `../../test/ethread_SUITE.erl` + `../../test/ethread_SUITE_data/ethread_tests.c` — a standalone C
  program (built by `ethread_SUITE_data/Makefile.src`, linked against `liberts_internal_r`,
  `$(ETHR_LIBS)`, `$(ERTS_INTERNAL_X_LIBS)`) that exercises the ethread mutex/atomic/spinlock/rwlock API.
- `../../test/erl_print_SUITE.erl` + `../../test/erl_print_SUITE_data/erl_print_tests.c` (with
  `character_test.h`, `integer_test.h`, `integer_64_test.h`, `snprintf_test.h`, `string_test.h`) —
  exercises `erl_printf.h` / `erl_printf_format.h`.
- Both suites are registered as `StaticLibSuites` in `../../test/system.dynspec`, so they run as part of
  the erts-level test run. They require a full built release (`TEST_NEEDS_RELEASE=true`); run from
  `ERL_TOP` via `make test`. **The suite `.erl`/`.c`/`Makefile.src` files live in `../../test/`, not
  here — read-only; escalate any change to `../../test/`.**
- No unit tests exist for the arch/impl headers as such; coverage comes indirectly from the emulator's
  own suites and from `DEBUG`/`ETHR_DEBUG` builds (which turn `ETHR_XCHK` on and activate
  `ETHR_ASSERT`).

## Routing Table
Every subdirectory is a small (2-6 header), highly formulaic implementation variant of `ethread` that is
selected by the macro chain in `ethread.h:373-410`. They are documented in the table below and
deliberately have **no separate CONTEXT.md**; only spawn agents into them for local edits.
Implementation bodies are never here — they are in `../../lib_src/{common,pthread,win}/`.

| Subdir | Headers | Targets / role |
| --- | --- | --- |
| `gcc/` | `ethread.h`, `ethr_atomic.h`, `ethr_dw_atomic.h`, `ethr_membar.h` | Native atomics via GCC `__atomic`/`__sync` builtins. Default for arches without a dedicated dir (aarch64/arm/riscv/…); also the final fall-through. `ethr_membar.h` re-includes `../i386/`, `../sparc32/`, `../ppc32/` barriers where needed. |
| `x86_64/` | `ethread.h` (24 lines) | Pure forwarder: `#include "../i386/ethread.h"`. Selected on `ETHR_SIZEOF_PTR==8 && __x86_64__`. |
| `i386/` | `ethread.h`, `atomic.h`, `ethr_dw_atomic.h`, `ethr_membar.h`, `spinlock.h`, `rwlock.h` | hand-written x86/x86-64 asm: `cmpxchg8b`/`cmpxchg` atomics, `xchg` spinlocks, x86 rwlocks. The real x86 implementation; selected by default on x86/x86_64. |
| `ppc32/` | `ethread.h`, `atomic.h`, `ethr_membar.h`, `spinlock.h`, `rwlock.h` | 32-bit PowerPC (`__powerpc__`/`__ppc__` && !`__powerpc64__`): `lwarx`/`stwcx.` atomics, `lwsync` barriers (runtime-probed). |
| `sparc32/` | `ethread.h`, `atomic.h`, `ethr_membar.h`, `spinlock.h`, `rwlock.h` | 32-bit SPARC (sparc-v9 asm: `cas`, `ldstub`, `membar`). |
| `sparc64/` | `ethread.h` (23 lines) | Pure forwarder: `#include "../sparc32/ethread.h"`. Selected on `ETHR_SIZEOF_PTR==8 && __sparc__ && __arch64__`. |
| `tile/` | `ethread.h`, `atomic.h`, `ethr_membar.h` | Tilera TILE64/TILEPro (`__tile__`), 32-bit only; reached via xcomp configs. No spinlock/rwlock dir — falls back. |
| `libatomic_ops/` | `ethread.h`, `ethr_atomic.h`, `ethr_dw_atomic.h`, `ethr_membar.h` | Uses the external **libatomic_ops** library (`atomic_ops.h`); chosen with `--with-libatomic_ops=PATH` or when no other native impl is available. |
| `pthread/` | `ethr_event.h` only | POSIX event (`ethr_event_*`) declarations; body in `../../lib_src/pthread/ethr_event.c`. The rest of the pthread ethread impl is `../../lib_src/pthread/ethread.c` — see Design Decisions. |
| `win/` | `ethread.h`, `ethr_atomic.h`, `ethr_dw_atomic.h`, `ethr_membar.h`, `ethr_event.h` | Native win32/MSVC atomics and barriers plus the Windows event declarations (`../../lib_src/win/ethr_event.c`). Not to be confused with the git-ignored `win32/` configure-output dir. |

Cross-references (read-only — escalate writes to this node):
- `../../lib_src/` → all out-of-line implementations of these headers
- `../../test/` → `ethread_SUITE`, `erl_print_SUITE` and their C data
- `../../../lib/common_test/test_server/ts_erl_config.erl` → consumer of the installed `.mk` fragments

## See Also
- `../../lib_src/` → `common/` (`ethr_aux.c`, `ethr_mutex.c`, `ethr_atomics.c`, `ethr_cbf.c`,
  `erl_misc_utils.c`, `erl_printf.c`, `erl_printf_format.c`), `pthread/`, `win/`,
  `utils/make_atomics_api` (generator of `ethr_atomics.h`), and the `release_spec` that installs this dir.
- `../` → the semi-public ERTS header layer (`erl_int_sizes_config.h`, `erl_fixed_size_int_types.h`).
- `../../emulator/beam/erl_threads.h` → the emulator's wrapper over ethread used by VM code;
  `../../emulator/beam/erl_internal_test.h` → separately installed into `include/internal/` by
  `../../emulator/Makefile.in`'s `release_spec`.
- `../../../make/autoconf/otp.m4` → `ERL_FIND_ETHR_LIB` and the `ETHR_*`/`ERTS_INTERNAL_X_LIBS` AC_SUBSTs
  behind `*.mk.in`; `../../configure.ac:122` and `:3946-3947` → how `<host>/*` are generated.
  Never edit `../../configure`/`../../config.h.in` directly;
  run `./otp_build update_configure` from `ERL_TOP`.
- `../../../HOWTO/INSTALL.md` §"Atomic Memory Operations and the VM" (~line 547) — user-facing account of
  the native-atomic requirement and fallbacks.
- `../../../xcomp/erl-xcomp-*.conf` → cross-compile configs (Tilera tilepro, powerpc, sparc) that
  exercise the non-x86 impl dirs.
