# yielding_c_fun/test — YCF test suite

## Intent
Regression test suite for YCF ("Yielding C Fun"), the C→C source-to-source rewriter in `..` that turns marked C functions into yieldable coroutines (used to build long-running yielding BIFs/NIFs in `erts/emulator`).
It is a plain bash script plus a corpus of small C programs — **not Common Test**, and not wired into the OTP `make test` machinery.
Nothing outside `yielding_c_fun/` consumes this directory; the standalone `../GNUmakefile` is the only driver.

## API Surface
- `test.sh` — the whole suite, 197 lines of bash (`set -e`, `set -x`), no build system of its own.
  Run it with `make test` from `..` (target `test: $(YCF_EXECUTABLE)` builds `../bin/yielding_c_fun.bin` then runs `./test/test.sh`), or invoke `./test/test.sh` directly once the tool is built.
  It is cwd-independent (it resolves its own directory) and takes **no arguments** — its lone positional parameter `$GC` is a leftover from the removed GC feature and is spliced into the YCF command line, so passing anything makes the run fail.
  It puts `$DIR/../bin` on `PATH` and mixes two names for the tool: the wrapper `yielding_c_fun` (tracked bash script that pre-checks each `.c` argument by compiling it with plain `cc` before exec'ing `yielding_c_fun.bin`) and the raw binary `yielding_c_fun.bin` (used for the three steps whose input cannot compile standalone yet — the generated-header/per-stack cases).
  All intermediates go to `tmp_dir/` (git-ignored, created by the script).
  The phases it asserts, in order:
  1. `-repeat` (parse + re-print) round-trips byte-identically (`cmp`) for every file in `SIMPLE_TEST_FILES`.
  2. `-yield` output is identical to the default (no-flag) output.
  3. Golden behaviour: for each of the 28 `SIMPLE_TEST_FILES`, transform with that file's flags + `-yield`, compile with clang, run, and `cmp` stdout against `examples/<name>.c.out`.
  4. `-header_file_name` + `-output_file_name` produce code+header that compile and link against a hand-written main (`examples/test_generated_header_file_main.c`).
  5. `-only_yielding_funs -static_aux_funs` produce an include file usable by the original `.c` unchanged.
  6. Two `-debug` negative tests: the compiled program must exit non-zero (pointer-to-stack detection), plus one negative test where YCF itself must exit non-zero (`ycf_cannot_transform_fun_call.c`).
  7. `-log_max_mem_usage <file>` creates the file.
- `examples/` — 33 example `.c` inputs and 30 committed `.c.out` golden-stdout files; the corpus doubles as the documentation/learning material (`../README.md` points at it, `../doc/thread_tutorial.md` is built on `thread_example.c`). See its own CONTEXT.md for the file→feature map.
- `examples/sha256_erlang_nif/` — self-contained rebar3 demo of YCF used to build a *yielding* NIF; its invocation in `test.sh` is commented out, so it is never run automatically.

## Constraints
- A `.c.out` file is the **byte-exact stdout of the compiled and run program** (`cmp`, so trailing newline matters), never generated C text.
- `SIMPLE_TEST_FILES` and `SIMPLE_TEST_FILES_YIELD_FUNS` in `test.sh` are positional parallel arrays and must stay index-aligned (28 entries each, today); the per-file flag strings use `-fnoauto`, `-f`, `-frec`.
- `CC=clang` and `CC_ARGS="-std=c99 -pedantic -Wall -g"` are hardcoded — the suite cannot run without clang, and examples must be C99/warning-clean.
- Anything passed to the `yielding_c_fun` wrapper must also compile as plain C with the default `cc` (the wrapper's precheck); this is why steps 3–5 use `yielding_c_fun.bin` directly.
- Test programs must be deterministic (no time, addresses or real threading) — `thread_example.c` simulates three cooperative "threads" in one state machine for exactly this reason.
- New examples must be added to both arrays at the same index and their `.out` must be captured from a real run.
- Only runs against the standalone build (`../bin/yielding_c_fun.bin`); the OTP-integrated build produces `../bin/$(TARGET)/yielding_c_fun` and never runs this suite.

## Design Decisions
- **Golden comparison of program stdout instead of a snapshot of generated C**: generated code is huge, formatting-sensitive and implementation-detail-heavy, so goldens stay small, readable, and immune to transform-internal churn. Consequence: the suite intentionally says nothing about the *text* of the generated code.
- **`-repeat` + self-`cmp`** pins the parser/printer round trip separately from the transform, catching lexer/pretty-printer regressions that behavioural goldens would miss (and making source formatting of the corpus load-bearing).
- **Examples double as documentation**: each corpus file is self-contained (`main()`, own allocator/freer, `#ifdef YCF_YIELD_CODE_GENERATED` dual mode) so a reader can copy it out and run YCF on it.
- **Negative tests express "must fail"** as `(set +e ; cmd ; [ $? -ne 0 ])`.
- Third-party NIF demo kept as a vendored subproject and excluded from the automatic run rather than adapted to the harness.

## Known Issues / Gotchas
- The suite has **no CI coverage**: no `.github/`, `scripts/` or `make/` file references `make test`/`test_all` for YCF; only `erts/lib_src/Makefile.in` builds the tool. Regressions surface only if someone runs it by hand.
- Legacy remnants in `test.sh`: `$GC` (positional arg from the removed GC support — see commit `63367d9f76` "ycf: Remove GC support", which deleted `../lib/simple_c_gc/`) and `RR=-rr` immediately followed by `RR=""`, which makes the `rr`-record branch of the wrapper script dead in practice.
- The `test_add_san` / `test_mem_san` / `test_ub_san` / `test_sanitizers` targets in `../GNUmakefile` do **not** actually sanitize YCF: they add `-fsanitize=…` via `CFLAGS`, but `../main_target.mk`'s `YCF_CFLAGS` filters `-fsanitize%` out deliberately ("YCF is a short lived tool leaking memory deliberately"), and `test.sh`'s `CC_ARGS` adds no sanitizer flags either. Their visible effect is building with clang plus the targets' own `-O00`/`-O02`-style flags.
- `examples/test_generated_header_file_main.c` hardcodes `#include "tmp_dir/tmp.h"` and is compiled with `-I<test dir>`; `examples/test_only_output_yielding_funs*.c` hardcode `#include "tmp.ycf.h"` with `-I tmp_dir`. Renaming `TMP_DIR`/`TMP_H_FILE`/`TMP_INC_FILE` in `test.sh` silently breaks those steps.
- `debug_ptr_to_stack.c` is exercised twice with different expectations: in the golden loop *without* `-debug` (it is in `SIMPLE_TEST_FILES`) and again in the `-debug` negative test. It also defines the `ycf_debug_get_stack_start()`/`ycf_debug_global_stack_top_ptr` hooks that `-debug`-generated code needs.
- The commented-out sha256 NIF block (~lines 190–194) cannot be enabled as-is: its `examples/sha256_erlang_nif/c_src/Makefile` still passes the removed `-use_gc` flag, so YCF prints a usage error and exits **0** without producing output, leaving an empty generated file and failing the demo build (see that node's CONTEXT.md).
- `../bin/yielding_c_fun` (wrapper) assumes the standalone layout and execs `$DIR/yielding_c_fun.bin`; it does not work in the `bin/$(TARGET)/` layout of an OTP build.
- Two files named `yielding_c_fun`/`yielding_c_fun.bin` plus a `yielding_c_fun.exe` copy in the bootstrap tree (`Makefile.in`) are easy to confuse when debugging.

## Notes for Agents
- Before changing generated **code text/formatting**: the suite will not notice — that is by design. Before changing **observable behaviour** (yield granularity, reduction accounting, trap protocol, destroy semantics): expect `.c.out` files to need regeneration, and regenerate them by running the transformed program, never by hand.
- `make test` from `..` is the way to validate any change to `../ycf_*.c`; combine with `../GNUmakefile`'s compiler-matrix target `test_gcc_clang_tcc` when touching portable C code.
- If you add a corpus file, follow the recipe in `examples/CONTEXT.md` (both arrays + `.out` from a real run).
- `../README.md` is the authoritative CLI/feature reference (`-f`, `-frec`, `-fnoauto`, `-fexternal`, `-yield`, `-repeat`, `-debug`, `-only_yielding_funs`, `-static_aux_funs`, `-header_file_name`, `-output_file_name`, `-log_max_mem_usage`); `../doc/thread_tutorial.md` is the introductory tutorial.
- `tmp_dir/` accumulates `tmp*.c`, `tmp.ycf.h`, `tmp.h`, `a.out` and the memory log; deleting it is always safe (the script recreates it).

## Dependencies
- bash, `cmp`, GNU make (for `../GNUmakefile`'s `test` target) and **clang** (hardcoded).
- A built YCF binary in `../bin` (`yielding_c_fun.bin`), produced by `make` in `..`.
- `cc` on `PATH` for the wrapper's standalone-compile precheck.
- Optional, only for `../GNUmakefile` extras: gcc, tcc, bmake, clang-tidy, clang-format, inotifywait, valgrind/ASan toolchains.
- For the (unexercised) sha256 demo: rebar3 plus Erlang/OTP with ERTS/erl_interface headers.

## Test Strategy
- `make test` in `..` is the suite; there is no per-file runner and no Common Test suite.
- `../GNUmakefile` variants: `test_add_san`, `test_mem_san`, `test_ub_san`, `test_sanitizers` (see Known Issues), `test_modern_cc` (`-O02`-style build), `test_32_bit` (`CC_32_BIT=1`, i.e. `-m32`), `test_bmake` (build via the generated `Makefile` with BSD make), `test_gcc_clang_tcc` (compiles the tool with gcc/clang/tcc `-Werror` but does **not** run this suite), `test_all` (all of the above), `run_test_continusly` (inotifywait loop), `clang_tidy`, `clang_format`.
- Coverage gaps: no test of `-print_ast`/`-print_symbols`/`-h`; no direct assertion on the text of generated code; `-fexternal` and multi-file/`#include` inputs untested; `ycf_cannot_transform_fun_call.c` only checks the exit code; error-message paths untested; the sha256 NIF demo and every window where the tool is run from an OTP-built `bin/$(TARGET)` are uncovered.

## Routing Table
- `examples/` → 33 example `.c` programs + 30 golden `.c.out` stdout files consumed by `test.sh` (own CONTEXT.md: file→feature map, how to add an example)
- `examples/sha256_erlang_nif/` → standalone rebar3 yielding-NIF demo project (own CONTEXT.md; **not** executed by `test.sh`)

## See Also
- `..` (`README.md`, `doc/thread_tutorial.md`, `GNUmakefile`, `main_target.mk`, `ycf_main.c`, `bin/yielding_c_fun`) → the YCF tool, its CLI documentation and its build/target definitions (read-only from here; escalate edits to the parent)
- `../Makefile` → generated, tracked thin wrapper kept only so non-GNU make can build the tool (regenerate via GNU make, do not hand-edit)
- `../../CONTEXT.md` → `erts/lib_src` node (runtime C support libraries); `../../../emulator/` → the consumer of YCF-generated yielding BIFs
