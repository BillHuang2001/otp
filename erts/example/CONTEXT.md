# erts/example/ — Documentation sample programs (NIF & driver examples)

## Intent
Small, hand-written, self-contained sample programs that demonstrate the two public ERTS C APIs — the NIF API (`erl_nif.h`) and the port-driver API (`erl_driver.h`) — plus one pure-Erlang example.
They are the "sample files" quoted by (and referenced from) the ERTS documentation, primarily `../doc/guides/driver.md` ("Ports and Port Drivers" chapter) and `../doc/guides/time_correction.md`.
They exist **for readers of the docs**, not for the build: nothing here is compiled, installed or tested by the OTP build. The directory is *not* in `erts/Makefile` `ERTSDIRS` (which is `etc epmd lib_src` + `preloaded`/`start_scripts`), so `make` at the top level never descends into it.

## API Surface (files)
- `matrix_nif.c` + `matrix_nif.erl` — NIF example: matrix arithmetic using **resource objects** (`enif_open_resource_type` with destructor, `enif_make_resource`/`enif_get_resource`, `ERL_NIF_INIT`). Erlang side is a module with `-nifs([...])`, `-on_load` and `erlang:load_nif("./matrix_nif", 0)` stubs.
- `next_perm.cc` + `next_perm.erl` — **C++** driver using STL `next_permutation`; demonstrates `driver_async`/`ready_async` and `driver_output_term` (`ErlDrvTermData` term building). The only C++ file in the directory.
- `pg_sync.c` + `pg_sync.erl` — synchronous port driver against PostgreSQL via **libpq**, using `ei` (`erl_interface`) to encode terms into binaries; driver entries `start`/`stop`/`control`, commands `$C`/`$D`/`$S`.
- `pg_async.c` + `pg_async.erl` — asynchronous variant of the above via the `ready_input`/`ready_output` entries (both set to one `ready_io` callback); this is the version the guide walks through.
- `pg_async2.c` + `pg_async2.erl` — further async variant: uses only `ready_input`, numeric commands `1`/`2`/`3`, and a different result encoding (nested lists of values/binaries built with `ei_x_encode_binary` + `PQgetlength`). **Not in the `Makefile` `TARGETS`** — only `make clean` touches its artifacts.
- `pg_encode.c`/`pg_encode.h` and the near-duplicate `pg_encode2.c`/`pg_encode2.h` — shared helpers `encode_ok` / `encode_error` / `encode_result` (libpq + `ei`) linked into the pg drivers; `pg_encode2.*` differs only in `encode_result`'s result construction.
- `time_compat.erl` — pure-Erlang module implementing the ERTS ≥ 7.0 time API (`monotonic_time`/`time_offset`/...) with a fallback to the older primitives so code can run on both old and new ERTS.
- `Makefile` — a **standalone** GNU makefile (includes `$(ERL_TOP)/make/target.mk` + `$(ERL_TOP)/make/$(TARGET)/otp.mk`) meant to be run **by hand from this directory**.
  Targets: `all` (default) builds `pg_sync.beam pg_async.beam pg_sync.so pg_async.so next_perm.so next_perm.beam time_compat.beam`; `clean` additionally removes `pg_async2.so pg_encode2.beam pg_async2.beam`, `*.o`, `core`, `erl_crash.dump`.
  `matrix_nif` has **no rule** — it must be compiled manually.

## Constraints
- These are documentation samples: keep them minimal, readable and stable. Code shown in `../doc/guides/*.md` is a verbatim snippet — a change here must be mirrored in the guide (and vice versa), so avoid gratuitous refactoring/renaming.
- Use **public headers only**: `erl_nif.h` (via `-I$(ERL_TOP)/erts/emulator/beam`) and `erl_driver.h`/`ei.h` (via `-I$(ERL_TOP)/lib/erl_interface/include`). No `internal/` or other private headers.
- Samples must stay valid against the *current* `erl_nif.h` / `erl_driver.h` API: API changes are expected to be reflected here (e.g. `matrix_nif.erl` adopts the `-nifs()` attribute; `matrix_nif.c` uses the env-taking `enif_open_resource_type`).
- Driver `DRIVER_INIT` functions must have external linkage (several files wrap it in `#ifdef __cplusplus extern "C"`) — they must **not** be `static`.
- `%.so`/`%.cc` rules use `$(CXX)`; everything else uses `$(CC)` with `-g -Wall -fpic`.
- Building the pg drivers requires `lib/erl_interface` to be built first (`EI_LIB = -L$(EI_ROOT)/obj/$(TARGET) -lei`) and libpq installed (`PQ_LIB = -lpq`).
- All files here carry SPDX/Apache-2.0 headers; `scripts/license-header.es` is not configured to skip `erts/example` (**`**/examples/**` in its exclusion list does not match this singular `example/`**), so new files need headers.
- Build artifacts (`*.beam`, `*.so`, `*.o`) are untracked; there is no local `.gitignore` (only the root `*.beam` rule applies) — keep the directory clean.

## Design Decisions
- The PostgreSQL/libpq driver is the running example for the port-driver chapter because it is a realistic case that shows both blocking (`pg_sync`) and non-blocking (`pg_async`, `ready_input`/`ready_output`) I/O against an external library.
- Multiple driver variants are kept side by side (`pg_sync`, `pg_async`, `pg_async2`, `next_perm`) to illustrate different driver styles (sync control, async I/O, `driver_async` worker threads, term output) rather than one parameterised sample.
- `matrix_nif` is a standalone NIF sample (resource objects) and is currently **not referenced by any generated doc** — only by historical release notes (`../doc/notes.md`). It is retained deliberately as a complete NIF reference example.
- `time_compat.erl` is the one file **published into the documentation build**: `../doc/Makefile` copies it to `../doc/assets/time_compat.erl` (the `assets` rule), and `../doc/guides/time_correction.md` links to it.

## Known Issues / Gotchas
- `matrix_nif.*` is not built by the `Makefile` and is not mentioned in any current doc/reference — do not expect `make` to produce `matrix_nif.so`.
- Comments in `pg_sync.c`, `pg_async.c`, `pg_async2.c` and `../doc/guides/driver.md` refer to a module `erl_pq_sync.erl` for the command-definition list; that module does not exist in the tree (the Erlang side is `pg_sync.erl` / `pg_async*.erl`).
- The `Makefile`'s pattern rule `%.so: %.o` hard-codes `pg_encode.o` (rather than deriving the encoder object from the target), so it only works for the `pg_*` drivers; and `pg_encode2.o`/`pg_async2.o` have their own explicit rule.
- The docs call this "the `sample` directory of `erts`" although the directory is named `example/` — same directory, different wording.
- `next_perm.so` is built from C++ without a `extern "C"` on the driver init beyond what the file already provides; keep the C/C++ split intact when editing.

## Notes for Agents
- Ignore this directory for build/API-impact analysis: it is not part of the shipped release (`erts-$(VSN)`), has no `release_spec` entry, is not in `ERTSDIRS`, and is not in any CI workflow — changes here cannot break the build, and a broken sample surfaces only when someone compiles it by hand or when the doc snippet drifts.
- If asked to "verify the example compiles", you need a configured/built tree: `lib/erl_interface/obj/$(TARGET)/libei.a` plus libpq headers; `make` in this directory is enough for the pg/next_perm targets.
- Do not document individual functions; the code is intentionally short and is explained in the ERTS guides.

## Dependencies
- GNU make with the OTP in-tree include files (`make/target.mk`, `make/$(TARGET)/otp.mk`) — i.e. `ERL_TOP` must be set and `configure` run.
- A C and a C++ compiler; `erl_interface` (`ei`, `-lei`) built in-tree; PostgreSQL client library `libpq` (`-lpq`) for the `pg_*` samples.
- At runtime the drivers are loaded with `erl_ddll:load_driver/2` from the current directory (`.`), so the `.so` must sit next to the Erlang module.

## Test Strategy
- None. There is no test suite, no test spec and no CI coverage for this directory; `erts`-level tests (`../test`, `../emulator/test`) do not reference these files.
- Manual verification only: `make` here (after building `lib/erl_interface` and installing libpq), then run the Erlang counterparts (`pg_sync.erl`, `pg_async.erl`, `next_perm.erl`, and `matrix_nif.so` compiled by hand).
- The only automated consumer is the documentation build: `../doc/Makefile` copies `time_compat.erl` into `erts-$(VSN)/doc/.../assets/`, so a syntax error there breaks the doc build, not the compiler build.

## See Also
- `../doc/guides/driver.md` → the Ports/Port Drivers chapter that quotes `pg_sync.c`, `pg_async.c`, `next_perm.cc` and their Erlang counterparts; `../doc/guides/time_correction.md` → uses `time_compat.erl`
- `../doc/references/erl_nif.md`, `../doc/references/erl_driver.md` → API reference for the headers these samples use
- `../../system/doc/tutorial/` → the Interoperability Tutorial, which has its **own** NIF/driver examples (`complex6_nif.c`, `cnode_c.c`, `ei.c`, ...); unrelated to this directory (sibling — read-only)
- `../../lib/kernel/examples/` (`gen_tcp_dist`, `erl_uds_dist`, `socket_sctp`) → the other doc-published example sources copied by `../doc/Makefile` (sibling — read-only)
- `../../lib/erl_interface/` → `ei` library and headers used by the pg drivers (sibling — read-only)

## Status
Complete: single CONTEXT.md covers the whole directory (no subdirectories).
