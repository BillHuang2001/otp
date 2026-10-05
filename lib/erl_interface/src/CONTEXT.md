# erl_interface C Sources (`src/`)

## Intent
The complete C implementation of the OTP `erl_interface` application: the `ei` library (`libei*.a` / `libei*.so`) and the `erl_call` utility.
Only the `ei` API is implemented here — the legacy `erl_*` / `liberl_interface` C API and the old `src/aux/`, `src/legacy/`, `src/not_used/` directories are not part of this tree.
`ei` is deliberately *reentrant / async-safe* (callable from signal handlers, no locks); thread safety of the surrounding connection state is handled in `connect/` and `misc/ei_locking.c`.
`erl_interface` has **no Erlang modules** — `{modules, []}` — so the `.app`/`.appup` files exist only so release/packaging machinery sees the application.
The application is mandatory: the top-level `configure` refuses `--without-erl_interface`.

## API Surface

### Build entry points
- `Makefile` — thin wrapper that re-invokes the generated `$(TARGET)/Makefile`; targets `opt` (default), `debug`, `shared`, `gcov`, `lcnt`, `valgrind`, `asan` (all skipped when `TERTIARY_BOOTSTRAP` is set) and `clean`, `depend`, `docs`, `release`, `release_docs`, `tests`, `release_tests`, `check`.
- `Makefile.in` — the real build logic; `../configure` preprocesses it into `src/$host/Makefile`, `../config.h.in` into `src/$host/config.h`, and `eidefs.mk.in` into `src/$host/eidefs.mk`.
- `eidefs.mk.in` — the thread variables substituted by `configure`: `EI_THREADS`, `THR_DEFS`, `THR_LIBS`, `EI_LDFLAGS`.
- `SOURCES = EISOURCES` = `CONNECTSRC` + `DECODESRC` + `ENCODESRC` + `EPMDSRC` + `MISCSRC` + `GLOBALSOURCES` + `ERTS_SOURCES`; `vpath %.c connect:encode:decode:misc:epmd:global:erts` means a new `.c` must be added to one of those lists to be compiled at all.
- Object dirs are `$(ERL_TOP)/lib/erl_interface/obj{.st,.mt,.md,.mdd}/$(TARGET)` — ST single-threaded, MT multi-threaded, MD/MDD Windows dynamic (debug); `obj/$(TARGET)` is used for `erl_call` and by external consumers (`../../erts/etc/common/Makefile.in`).
- Build outputs: `lib/$(TARGET)/libei*.a|.so` and `bin/$(TARGET)/erl_call` (all git-ignored; `bin`, `obj.st`, `obj.mt`, `obj` via `../.gitignore`, `obj.md`/per-host dirs via the root `.gitignore`).
- Library naming: with threads the default is MT `libei` (plus `libei_st`, and `libei_md`/`libei_mdd` on Windows); with `--disable-threads` only the ST `libei`/`libei_st` are built. `--enable-ei-dynamic-lib` (default: no) sets `DYNAMIC_LIB` and adds the shared libs.
- `erts` target copies `../../erts/emulator/beam/erl_md5.{c,h}` into the generated, git-ignored `src/erts/` — `ERTS_SOURCES = erts/erl_md5.c` supplies MD5 for the distribution handshake digest.
- `depend` target generates `src/$host/depend.mk` by running `$(CC) -MM` over `SOURCES` and `-include`s it; `ERTS_SKIP_DEPEND=true` skips generation (but still runs `erts`).
- `check` target builds the "fake programs" from `prog/` against the just-built library — a compile/link smoke test (see Known Issues).

### Application resource files
- `erl_interface.app.src` / `erl_interface.appup.src` — templates; `sed` substitutes `%VSN%` → `$(ERL_INTERFACE_VSN)` to produce `../ebin/erl_interface.app(.appup)`.
- Version source is `../vsn.mk` (`EI_VSN` = `ERL_INTERFACE_VSN`, currently 5.8.2).

### Install / release
- `release` (depends on `opt`) installs into `$(RELEASE_PATH)/lib/erl_interface-$(EI_VSN)/{include,lib,bin,ebin,src/<subdir>}` and into `$(RELEASE_PATH)/usr/{include,lib}`; `RELEASE_PATH` is `release/$(TARGET)` or `$TESTROOT`.
- Installed headers (`HEADERS`) are only `../include/ei.h`, `../include/ei_connect.h`, `../include/eicode.h`; every header in this directory is internal and is shipped as source only.
- `bin/$(TARGET)/erl_call` goes to `$(RELSYSDIR)/bin`; `../../erts/etc/common/Makefile.in` further copies it (and links `-lei` from `obj/$(TARGET)`) into the release `bin`.
- `libodbc`'s `c_src` links `-lei` and adds `-I…/erl_interface/include/$(TARGET)`, an in-tree consumer of this library.

## Constraints
- Library sources (in `connect/`, `decode/`, `encode/`, `epmd/`, `global/`, `misc/`) must include `"eidef.h"` as the first include: it pulls in the generated `config.h`, `NULL`, the `ei.h` API, the `uint8/uint16/uint32/int8/int16/int32` typedefs, `EISMALLBUF`, `socklen_t` fallback, `isfinite` shims, `EI_FALLTHROUGH()`, `ASSERT` and `EI_UNDEF`.
  - Exceptions: `misc/ei_init.c` and `misc/ei_compat.c` include `"ei.h"` directly; `prog/` programs may (and do) include `"config.h"`.
- `ei` code must stay reentrant / async-signal-safe: no locks inside `ei`; locking (`ei_mutex_*` from `misc/ei_locking.c`) is confined to the fd→cnode registry in `connect/ei_connect.c`.
- Use the fixed-width typedefs for anything placed on the wire — never assume `short`/`int`/`long` widths — and never pass `char` values > 127 to `isupper()`/`isalpha()`-style macros.
- Avoid globals (thread-safety, and VxWorks makes process globals OS-global); the sanctioned ones are `ei_x_extra` (mutable, `misc/ei_x_encode.c`), `errno`/`__erl_errno` (per-thread emulation in `misc/ei_pthreads.c`) and the library-internal `ei_tracelevel`, `ei_sockets_lock`, `ei_plugin_socket_impl__` (declared in `misc/*.h`, defined in `connect/ei_connect.c` / `misc/ei_portio.c`).
- Keep the long-long encoders/decoders in their own translation units (`encode_longlong.c`, `encode_ulonglong.c`, `decode_longlong.c`, `decode_ulonglong.c`) so users of the 32-bit functions do not acquire a `libgcc.a` `__ashldi3` dependency (`README.internal`, "The long long story"); under `-DEI_64BIT` those same files also host `ei_encode_long`/`ei_encode_ulong`/`ei_decode_long`/`ei_decode_ulong`.
- Wire data must match the ERTS external-term/distribution encoding: tag bytes come from `../include/ei.h`, byte-order helpers from `misc/putget.h` (big-endian `get*be` for the term format).
- Never edit generated files (`../configure`, `src/$host/*`); edit `Makefile.in`, `config.h.in`, `eidefs.mk.in` (and `../configure.ac`, then `./otp_build update_configure`).

## Notes for Agents
- `README.internal` is the design-rationale document (include order, globals policy, "The long long story", the `make check` intent) but its inventory is stale: it describes removed `src/aux/`, `src/legacy/`, `src/not_used/`, `aclocal.m4`/libtool/`configure.in`, `include/erl_interface.h`, `prog/erl_fake_prog.c` and a global `ei_trace_distribution` — none of these exist.
- One C file ≈ one object file by design (static-link granularity), which is why nearly every `ei_encode_*`/`ei_decode_*` function has its own file.
- `prog/ei_fake_prog.c` references every public `ei` symbol and is the canary for API/header changes — update it when adding a public function.
- Internal cross-directory entry points worth knowing: `misc/eidef.h` (mandatory first include), `misc/putget.h` (wire macros), `misc/ei_portio.h` (USS (user-supplied socket) / socket-callback indirection used by `connect/` and `epmd/`), `connect/eirecv.c`'s `ei_recv_internal` (also used by `global/` and `epmd/`).

## Known Issues
- `HEADERS` omits `../include/ei_global.h`, so that header is not installed to `…/include` or `usr/include`; the `ei_global_*` functions stay reachable only because `../include/ei.h` duplicates their prototypes (see `global/CONTEXT.md`).
- `check`/`FAKE_TARGETS` still reference `prog/erl_fake_prog.c` and `-lerl_interface[_st|_r]`, none of which exist any more, so those targets cannot be satisfied; `NEVERUSED` also lists non-existent `whereis.c`, `ei_send.c`, `ei_send_reg.c`, `send_link.c`, and `release` creates an unused `src/auxdir`. `src/Makefile`'s `check` is not invoked by the OTP build or CI (it is unrelated to `otp_build check` / `../../../scripts/otp_build_check`).
- `misc/ei_compat.c`'s `ei_set_compat_rel()` only assigns a static that nothing reads — the public call is a no-op.
- Directory-specific gotchas (no input bounds checking in the decoders, `ms == 0` meaning "block forever" for every `_tmo` variant, bignum encode/decode sign+endianness mismatches, fd-table dereference before `ei_*init*()` crashing instead of returning `EBADF`) are recorded in the child `CONTEXT.md` files below.

## Dependencies
- C compiler plus `ar`/`ranlib` (configure errors out without `ar`), sockets and whatever `@LIBS@`/`WIN_SOCKET` resolves to on the platform.
- Threads are optional: pthreads (or Win32 threads) enable the MT/MD/MDD variants; `--disable-threads` builds only the ST library.
- GNU MP 4.1+ only for `--with-gmp`; it is never linked into the shipped library (used by the bignum paths and `make check`).
- GNU autoconf 2.72 to regenerate `../configure` from `../configure.ac` (`m4_include`s `../../make/autoconf/otp.m4`); `configure` must have been run before `make`.
- Build-time input from `../../erts/emulator/beam/erl_md5.{c,h}` (copied by the `erts` target).
- `erl_call` needs a running `epmd` and a reachable Erlang node at runtime.

## Test Strategy
- `src/` contains no tests: `tests:` and `release_tests:` are empty in `Makefile.in`. Tests live in `../test/` and drive small C programs in `<suite>_data/` through `../test/runner.erl` via `open_port` term exchange.
- `ei_encode_SUITE`, `ei_decode_SUITE`, `ei_decode_encode_SUITE`, `ei_connect_SUITE`, `ei_accept_SUITE`, `ei_tmo_SUITE`, `ei_format_SUITE`, `ei_print_SUITE`, `ei_global_SUITE`, `erl_call_SUITE` are the coverage of this code; each child `CONTEXT.md` names the suites covering it.
- Building all four variants (ST/MT/MD/MDD) is required for full Windows coverage; on Unix only ST+MT are normally built.
- `make check` here is only a compile/link smoke test and is currently broken for the `erl_fake_prog*` targets (see Known Issues).

## Routing Table
| Directory | Description |
| --- | --- |
| `connect/` | Distribution protocol client: node/cookie init, epmd-based connect + handshake, listen/accept, `ei_send*`/`ei_receive*`/`ei_rpc*`, host resolution (§ `ei_connect.h`/`ei.h` API) |
| `decode/` | `ei_decode_*` — external term format decoding, one function per file, plus the `ei_skip_term` helper (`decode_skip.h`) |
| `encode/` | `ei_encode_*` — external term format encoding into a caller-managed buffer (the `ei_x_*` dynamic-buffer wrappers are mostly in `misc/`) |
| `epmd/` | `ei_publish*`/`ei_unpublish*` — the client protocol to the external `epmd` daemon (`ei_epmd.h` holds the wire constants) |
| `global/` | `ei_global_names/whereis/register/unregister` — client of the Erlang `global` name server over distribution |
| `misc/` | Everything shared: `eidef.h`/`eiext.h`/`putget.h`/`ei_internal.h`, `ei_x_*` buffer API, `ei_print_term`/`ei_format`, malloc/lock/errno/portability layers, socket-callback layer, `ei_trace`/`show_msg` |
| `prog/` | `erl_call.c` (the `erl_call` CLI) and `ei_fake_prog.c` (public-API link/header smoke test used by `check`) |
| `../include/` | The public installed headers (`ei.h`, `ei_connect.h`, `eicode.h`, plus uninstalled `ei_global.h`) — sibling, read-only from here |
| `../test/` | Common Test suites and their C drivers exercising this code — sibling, read-only from here |
| `../doc/` | `docs.exs`/references/guides for `ei`, `ei_connect`, `ei_global`, `erl_call` — sibling, read-only from here |

## See Also
- `../configure.ac`, `../config.h.in`, `../Makefile` — app-level configure and build wiring for this `src/` tree.
- `../../erts/emulator/beam/` — canonical external term format (`external.c`) and distribution (`dist.c`) encoding this library must match; `erl_md5.c` is copied from there.
- `../../erts/epmd/src/` — the server side of the `epmd` protocol implemented in `epmd/`.
- `../../erts/etc/common/Makefile.in` — installs `erl_call` into the release `bin` and links `-lei` from `obj/$(TARGET)`.
- `../../lib/kernel/src/global.erl` — the server side of the `global/` client.
- `../../../make/autoconf/otp.m4`, `../../../make/target.mk`, `../../../make/otp.mk` — shared build/configure infrastructure used by `Makefile.in`.
- `../doc/references/ei.md`, `ei_connect.md`, `ei_global.md`, `erl_call_cmd.md` — the user-facing documentation of this API surface.
