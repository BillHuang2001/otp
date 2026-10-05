# sha256_erlang_nif — yielding NIF demo

## Intent
Self-contained rebar3 project (OTP app `sha256_nif`, v0.1.0) that demonstrates how to implement a **yielding Erlang NIF** with Yielding C Fun: a vendored SHA-256 C implementation is transformed by YCF into a `*_ycf_gen_yielding` / `*_ycf_gen_continue` / `*_ycf_gen_destroy` state machine, and a hand-written `erl_nif` module drives that state machine with `enif_schedule_nif` so a long hash reschedules the calling process instead of blocking a scheduler thread.
It is primarily *documentation by example* — referenced from `../../README.md` ("This Erlang NIF example shows how one can use YCF to write a yielding Erlang NIF library") — not part of the OTP build or test run.

## API Surface
- `src/sha256_nif.erl` — Erlang side: exports `sha256/1`, `-on_load(init/0)` loads `priv/sha256_erlang_nif`; `sha256/1` before load raises `{not_loaded, ...}`.
- `src/sha256_nif.app.src` — app metadata only (description/vsn/licences, `applications = [kernel, stdlib]`).
- `c_src/sha256_nif.c` — the NIF: `ERL_NIF_INIT(sha256_nif, ...)`, one NIF `sha256/1`, plus the YCF↔NIF integration plumbing (a `sha256continuation` resource holding work env + trap state + output binary, an `erl_nif` resource destructor that calls `calc_sha_256_ycf_gen_destroy`, and a reduction budget of `REDUCTIONS_UNTIL_YCF_YIELD()` = 200*300*8).
- `c_src/sha-2/sha-256.c` — YCF-adapted copy of the upstream implementation; the **input** to the YCF transform (not compiled directly, see `c_src/Makefile`).
- `c_src/sha-2/sha-256.h` — the only public header of the vendored lib: `void calc_sha_256(uint8_t hash[], const void *input, size_t len)`; included by `sha256_nif.c` for the un-yieldable declaration.
- `test/basic_SUITE.erl` — Common Test suite: `test1` hashes growing inputs (1024 bsl V, V=1..15) and asserts the NIF result equals `crypto:hash/2` while printing both timings; `test2` asserts equality with `crypto:hash(sha256, "hej")`.
- `Makefile` (rebar3 wrapper: `build`, `test`, `docs`, `release`), `rebar.config`, `README.md`, `LICENSE` (Apache-2.0).
- Generated at build time and git-ignored: `c_src/gen_yielding_sha_256.c`, `c_src/gen_yielding_sha_256.h`, `priv/sha256_erlang_nif.so`, `_build/`.

## Constraints
- The shared-library basename is derived from the **directory name**: `c_src/Makefile` sets `PROJECT = $(notdir $(abspath $(CURDIR)/..))`, producing `priv/sha256_erlang_nif.so`; `src/sha256_nif.erl` hardcodes `?LIBNAME = sha256_erlang_nif`. Renaming this directory breaks NIF loading.
- `sha-2/sha-256.c` must stay YCF-friendly; it deliberately differs from the pristine upstream copy `sha-2/sha-256_orginal.c` via an `#ifdef YCF_YIELD_CODE_GENERATED` include guard, a `YCF_STACK_ALLOC(X)` indirection (so the transformer can substitute its stack allocator), and hoisted local declarations.
- `c_src/sha256_nif.c` forward-declares `struct buffer_state;` **before** `#include "gen_yielding_sha_256.h"`: the generated header needs incomplete types used by yielded function parameters to be declared first.
- Only `-f calc_sha_256 -f calc_chunk` are asked to yield (auto-yield mode); `sha-256.h` is not regenerated, so the generated code mirrors the hand-written header.
- Never edit generated `gen_yielding_sha_256.*`, `priv/`, or `_build/`.

## Known Issues
- **The demo is stale and does not build as-is.** `c_src/Makefile` generates the transformed code with `-use_gc`, a YCF option that no longer exists (GC support was removed from YCF in OTP commit 63367d9f76). The current compiler accepts no unknown option: it stops parsing at `-use_gc` and exits 1 with `ERROR: Expected at least one "(-f|-frec|-fnoauto) function_name" argument`, so the `gen_yielding_sha_256.c` rule fails. Removing `-use_gc` is required; the rest of the pipeline (flag list, generated function names, and the 9-argument `*_ycf_gen_yielding` call in the NIF) still matches the current generator.
- The `c_src/Makefile` generation rule calls `../../../../bin/yielding_c_fun`, the tracked bash wrapper, which execs `bin/yielding_c_fun.bin` from its own directory. The in-tree OTP build instead produces `yielding_c_fun/bin/$(TARGET)/yielding_c_fun`, so this path only resolves after a **standalone** YCF build in `erts/lib_src/yielding_c_fun/`.
- `CODE_SOURCES`-style filtering in `c_src/Makefile` relies on `grep -v "./sha-2/..."` patterns applied to `find` output; the leading `.` acts as a regex wildcard, which is what makes the excludes work on absolute paths — do not "fix" these to literal `./` paths without re-checking.
- In `src/sha256_nif.erl` the `{error, bad_name}` fallback of `code:priv_dir/1` builds `priv/sha256_erlang_nif` (no platform shared-library extension), so it fails unless the app is found in the code path (the normal rebar3 case).

## Notes for Agents
- Relative path math: from `c_src/`, `../../../../bin` is `erts/lib_src/yielding_c_fun/bin`.
- The vendored `sha-2/` files (`Makefile`, `test.c`, `.travis.yml`, `.gitignore`) are upstream leftovers; `c_src/Makefile` excludes `sha-2/sha-256.c`, `sha-2/sha-256_orginal.c`, `sha-2/test.c` and `gen_yielding_sha_256.c` from `SOURCES`, so only `sha256_nif.c` plus the generated file are compiled.
- `sha-256_orginal.c` is the unmodified upstream source kept for diffing against the YCF-adapted `sha-256.c`.
- `rebar.config` declares the C build through pre/post hooks (`make -C c_src`, `gmake` on FreeBSD) and repeats `erl_opts`/`deps` twice (harmless).

## Dependencies
- A built YCF compiler at `erts/lib_src/yielding_c_fun/bin/yielding_c_fun.bin` (standalone `make` in that directory; needs a C compiler and GNU make).
- A working Erlang/OTP installation with `erl` on `PATH`: `c_src/Makefile` shells out to `erl -noshell` to discover `erts-*/include`, the `erl_interface` include dir and lib dir, and links against `libei` (`-lei`).
- `rebar3` on `PATH` (or `./rebar3` in this directory); the wrapper `Makefile` also uses `rebar3 edoc` and `rebar3 release`.
- `gcc`/`cc` and GNU make (Darwin/FreeBSD/Linux branches in `c_src/Makefile`; FreeBSD uses `gmake`).
- The `crypto` OTP application at test time (used by both test cases but not declared in `src/sha256_nif.app.src`).

## Test Strategy
- `rebar3 ct` (or `make test`) compiles `c_src` through the `rebar.config` pre-hook, then runs `test/basic_SUITE.erl`; `rebar3 clean` triggers the `make -C c_src clean` post-hook. Test cases compare the NIF output against `crypto:hash/2`; `test1` is timing-oriented and hashes up to ~32 MB, so it is slow and memory-hungry.
- This example is **not part of any CI/test run**: nothing in the OTP build (`erts/lib_src/Makefile.in`, `erts/test`, `erts/emulator/test`) builds or runs it, and the invocation in the sibling `../test.sh` is commented out ("Uncomment to the test the Erlang NIF example").
- Useful as a manual/integration check only after the `-use_gc` staleness above is dealt with.

## Routing Table
- `src/` → Erlang module and `.app.src` for the demo app
- `c_src/` → NIF C source and the Makefile that runs YCF and links `priv/sha256_erlang_nif.so`
- `c_src/sha-2/` → vendored public-domain SHA-256 from https://github.com/amosnier/sha-2 (Unlicense, see `LICENSE`; upstream revision recorded in `GIT_VERSION`) — third-party, treat as read-only
- `test/` → Common Test suite for the NIF
- `../` (sibling `examples/`) → all other YCF example C files and `.c.out` golden files, driven by `../test.sh`; documented at the `test/` level (read-only from here, escalate writes to the parent)

## Status
Complete. The example's C/Erlang integration remains a valid reference even though its build flag list has gone stale.
