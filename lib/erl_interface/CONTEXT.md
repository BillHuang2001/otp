# erl_interface — C Library for Erlang Communication

## Intent
The OTP application providing the `ei` C library (`libei*.a` / `libei*.so`) and the `erl_call` command-line utility.
`ei` is the supported way for C code to encode/decode the Erlang external term format and to speak the Erlang distribution protocol (C-nodes, port and driver programs).
`erl_interface` is C-only: it declares `{modules, []}`, so there are no Erlang runtime modules — `ebin/erl_interface.app` exists only so release/packaging machinery sees the application.
The legacy `erl_*` C API is gone from this tree: no `liberl_interface`, no `include/erl_interface.h`, no `src/legacy/`; `ei` is the whole interface, and `include/ei_connect.h` / `include/eicode.h` survive only as stubs forwarding to `ei.h`.
The application is mandatory for a normal build (the top-level `configure` refuses `--without-erl_interface`) and is part of `otp_build`'s bootstrap app list.

## API Surface
- `Makefile` — app build driver: `SUB_DIRECTORIES = src doc`; includes `$(ERL_TOP)/make/otp_subdir.mk` and `make/app_targets.mk`; `make` builds the C library and docs, `make test` runs the Common Test suites in `test/`.
- `configure` / `configure.ac` — this app's own autoconf (required `AC_PREREQ(2.72)`, `m4_include(otp.m4)` from `make/autoconf/`); `configure` is generated and is invoked by the top-level `configure`, so change `configure.ac` then run `./otp_build update_configure`.
  Options: `--disable-threads` (build only single-threaded libs), `--enable-ei-dynamic-lib` (default no), `--disable-mask-real-errno`, `--with-gmp[=PATH]` (bignum paths / `make check` only).
- `config.h.in` — autoconf config template: type sizes, `HAVE_*` header/function probes, `EI_HIDE_REAL_ERRNO`, thread selection; consumed as `src/$host/config.h`.
- `vsn.mk` — the app's version: `EI_VSN = 5.8.2` and `ERL_INTERFACE_VSN = $(EI_VSN)` (this is why `make/fixup_doc_links.sh` special-cases the app).
- Build products: `lib/$(TARGET)/libei*.a|.so`, `bin/$(TARGET)/erl_call`, `ebin/erl_interface.app(.appup)` (from `src/*.app.src` with `%VSN%` substituted).
- Install layout: `$(RELEASE_PATH)/lib/erl_interface-$(EI_VSN)/{include,lib,bin,ebin,src/…}` plus headers/libs in `$(RELEASE_PATH)/usr/{include,lib}`; `../../erts/etc/common/Makefile.in` copies `erl_call` into the release `bin` (`RELEASE_PATH` is `release/$(TARGET)` or `$TESTROOT`).
- In-tree consumers of the library and headers: `../../erts/etc/common/` (`erl_call` copy, `-lei` from `obj/$(TARGET)`), `../../erts/example/` (C-node examples), `../odbc/c_src/`, `../../system/doc/tutorial/cnode*.c`.
- `AUTHORS`.

## Constraints
- Never edit generated files: `configure` (edit `configure.ac`, then `./otp_build update_configure`), `src/$host/{Makefile,config.h,eidefs.mk}`, `ebin/*.app*`.
- Wire compatibility is a hard requirement: the term/distribution encoding produced here must match erts (`../../erts/emulator/beam/external.c`, `dist.c`).
- Public API and ABI stability: `include/ei.h` is the contract for external C programs; a new public function must be declared in `include/`, implemented under `src/`, documented in `doc/references/`, and referenced by `src/prog/ei_fake_prog.c`.
- `ei` itself must stay reentrant / async-signal-safe (no locks), while the library must remain usable from multiple threads (thread support is confined to the connection state).
- C and C++ consumers must both compile (headers are `extern "C"`-wrapped; the fake programs are also built as C++).
- Build outputs (`obj*`, `bin/`, `lib/`, `ebin/` contents, `doc/html/`, `doc/man*`) are never edited by hand.

## Notes for Agents
- The layout diverges from the standard OTP app schema: no Erlang modules, no `include/*.hrl`, `src/` is a tree of C sources grouped by function rather than a flat `src/`, and `ebin/` holds manifests only.
- `src/README.internal` is the design-rationale document (include order, globals policy, "the long long story", `make check` intent) but its file inventory is stale — it still describes the removed `src/aux/`, `src/legacy/`, `src/not_used/`, `aclocal.m4`/libtool, `include/erl_interface.h` and `prog/erl_fake_prog.c`.
- A new C file is only compiled if it is added to the relevant `*SRC` list in `src/Makefile.in` (the build uses `vpath`, not wildcards).
- `priv/`, `ebin/` and `src/.gitignore` exist as empty placeholders (only `.gitignore` tracked); `bin/`, `lib/` and `obj*/` do not exist in the source tree — they are created by the build and are git-ignored.
- Dependencies on this app are declared nowhere in Erlang: other applications consume it by linking `libei` and including `ei.h`, not via `.app` dependencies.
- Windows/VC/MinGW targets are supported through the `obj.md`/`obj.mdd` variants and the `win32`/`win64` autoconf cache files in `make/autoconf/`.

## Dependencies
- C toolchain: compiler, `ar`, `ranlib` (configure errors out without `ar`), GNU make; sockets/`nsl` libraries as probed by `configure`.
- Threads are optional: pthreads (or Win32 threads) enable the MT/MD/MDD library variants; `--disable-threads` yields ST-only builds.
- GNU MP 4.1+ only with `--with-gmp`; it is never linked into the shipped library.
- Runtime: `erl_call` needs a running `epmd` and a reachable Erlang node; the distribution tests need a resolvable hostname.
- GNU autoconf 2.72 to regenerate `configure`; `configure` must have been run before `make`.

## Test Strategy
- `test/` holds Common Test suites that compile and drive small C programs (see `test/CONTEXT.md`); run `make test` from this directory (or `make erl_interface_test` from `ERL_TOP`), with `SPEC_POSTFIX=smoke` for the smoke spec, and `make test ARGS="-suite ei_decode_SUITE -case …"` for single cases.
- Prerequisites: the C library must be built first — `obj[.debug]/<target>/libei.a` and `src/<target>/eidefs.mk`; `erl_call_SUITE` additionally needs `bin/<system_architecture>/erl_call`.
- `ei_accept`, `ei_connect`, `ei_tmo`, `ei_global` and `ei_decode_encode` suites need a running `epmd` and hostname resolution; `ei_decode`, `ei_encode`, `ei_format` and `ei_print` only exchange terms over a port.
- This is the only OTP app using the `Makefile.first`/`Makefile.src` test-server mechanism; see `../../HOWTO/TESTING.md`.
- `make check` in `src/` is a compile/link smoke test, not part of the OTP build or CI, and its `erl_fake_prog*` targets are broken (see `src/CONTEXT.md`).

## Routing Table
- `src/` → the entire C implementation: `ei` sources grouped in `connect/`, `decode/`, `encode/`, `epmd/`, `global/`, `misc/`, plus `prog/` (`erl_call`, `ei_fake_prog`) — see `src/CONTEXT.md`
- `include/` → the public, installed C headers and the ABI contract (`ei.h`, `ei_connect.h`, `eicode.h`, uninstalled `ei_global.h`) — see `include/CONTEXT.md`
- `test/` → Common Test suites, their `*_SUITE_data/` C drivers, `runner.erl` and the installed test specs — see `test/CONTEXT.md`
- `doc/` → hand-written markdown guides and reference pages (`ei`, `ei_connect`, `ei_global`, `erl_call`) plus `notes.md` release notes — see `doc/CONTEXT.md`
- top-level files → `configure.ac`/`configure`, `config.h.in`, `Makefile`, `vsn.mk`, `AUTHORS` (no separate CONTEXT.md; described above)
- `ebin/`, `priv/` → placeholders for build output / unused runtime data (only `.gitignore` tracked)
- `bin/`, `lib/`, `obj*/` → build outputs created by `make`, not present in the source tree
- `../odbc/c_src/`, `../../erts/etc/common/`, `../../erts/example/`, `../../system/doc/tutorial/` → in-tree consumers of `libei` and `ei.h` (other apps/directories — read-only, escalate writes to their owners)

## See Also
- `../CONTEXT.md` — OTP library applications overview and build order.
- `../../erts/emulator/beam/external.c`, `../../erts/emulator/beam/dist.c` — canonical external term format and distribution encoding this library must match.
- `../../erts/epmd/src/` — the server side of the `epmd` protocol implemented in `src/epmd/`.
- `../../../make/autoconf/otp.m4`, `../../make/doc.mk`, `../../make/fixup_doc_links.sh` — shared build/configure/doc machinery used here.
- `../../HOWTO/INSTALL.md`, `../../HOWTO/TESTING.md` — configuring/building and the test-server mechanism.
