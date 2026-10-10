# yielding_c_fun/test/examples — YCF example & regression corpus

## Intent
Golden-file regression corpus **and** the learning material for YCF (the C→C source-to-source rewriter in `..`).
Each `*.c` file here isolates one YCF feature (a macro form, a declaration kind, a control-statement shape, a CLI mode) so that a regression in the transform shows up as one failing `cmp`, and so a reader can find a minimal example of any construct.
`../README.md` points readers here; `../doc/thread_tutorial.md` is built around `thread_example.c`.

## API Surface
### Anatomy of a well-formed example
- Self-contained single translation unit: `#include <stdio.h>`, `<stdlib.h>`, its own `main()`, and its own allocator callbacks matching the YCF alloc/free function types (`void* allocator(size_t, void*)`, `void freer(void*, void*)`).
- `#define YCF_YIELD()` (or `YCF_YIELD_NO_REDS()` / `YCF_CONSUME_REDS(X)` / `YCF_STACK_ALLOC(X)`) at the top so the file also compiles unchanged by a normal C compiler.
- The calling code is wrapped in `#ifdef YCF_YIELD_CODE_GENERATED`: that branch calls the generated entry points `<fun>_ycf_gen_yielding(...)`, `<fun>_ycf_gen_continue(...)`, `<fun>_ycf_gen_destroy(...)`; the `#else` branch calls the original function. `wb`/state out-param non-NULL == "trapped/yielded".
- `main()` prints its progress/result and returns 0 on success (non-zero only for the intentionally-failing cases).
- `<name>.c.out` is the expected **stdout of the compiled+run program** (see Constraints), not generated C code.

### Golden-stdout examples (28, order matters — positional pairing with `../test.sh`)
Every entry is transformed with the CLI flags in the right-hand column (`yielding_c_fun <flags> -yield <file>`), compiled, run and `cmp`-ed against its `.c.out`.

| # | File | YCF flags in `../test.sh` | Feature covered |
|---|------|---------------------------|-----------------|
| 1 | `simple_yield.c` | `-fnoauto fun` | Minimal yield: straight-line code, one `YCF_YIELD()` |
| 2 | `multi_scope_yield.c` | `-fnoauto fun` | Locals in several nested scopes across a yield |
| 3 | `nested_loop_yield.c` | `-fnoauto fun` | Yield inside an inner loop of a nested loop nest |
| 4 | `void_ret_fun.c` | `-fnoauto fun` | `void`-returning yieldable fun; result written through an out-param |
| 5 | `declarations.c` | `-fnoauto fun` | Wide range of parameter/local declarations saved+restored |
| 6 | `void_param.c` | `-fnoauto fun` | Parameter list `(void)` |
| 7 | `simple_fun_call.c` | `-fnoauto fun` | Call to a plain (non-yieldable) function |
| 8 | `control_statements.c` | `-fnoauto fun` | Breadth of `if/else/for/while/do-while/switch/goto` |
| 9 | `simple_yielding_fun_call.c` | `-fnoauto fun -fnoauto sub_fun_{1,2,3}` | One yieldable fun calling other yieldable funs |
| 10 | `yielding_mutual_recursion.c` | `-fnoauto A -fnoauto B` | Mutual recursion between two yieldable funs |
| 11 | `stack_array.c` | `-fnoauto fun` | Stack arrays (incl. 2-D and array-of-pointers) surviving yields; pointer passed via extra context |
| 12 | `custom_code_save_restore_yield_state.c` | `-fnoauto fun -fnoauto fun2` | Hooks, **style 2** (`/*special_code_start:EVENT*/ if(0){...} /*special_code_end*/`), all events incl. `ON_DESTROY_STATE`; also exercises the destroy path |
| 13 | `custom_code_save_restore_yield_state_alt_syntax.c` | `-fnoauto fun` | Hooks, **style 1** (`YCF_SPECIAL_CODE_START(EVENT)` / `YCF_SPECIAL_CODE_END()`) |
| 14 | `destroy_while_yielded.c` | `-fnoauto fun` | `fun_ycf_gen_destroy(wb)` on a trapped state (destroy-while-yielded unwind) |
| 15 | `consume_reds.c` | `-fnoauto fun` | `YCF_CONSUME_REDS(10)` with a constant |
| 16 | `consume_reds_variable.c` | `-fnoauto fun` | `YCF_CONSUME_REDS` with a non-constant expression |
| 17 | `nested_call_consume_reds.c` | `-fnoauto fun -fnoauto sub_fun` | `YCF_CONSUME_REDS` inside a nested yieldable call |
| 18 | `auto_yield.c` | `-frec fun -frec rec_inc` | `-frec` auto-yield: recursion + all loop kinds auto-yield without explicit macros |
| 19 | `yield_with_struct.c` | `-fnoauto fun` | Struct-typed locals across a yield |
| 20 | `const_defenition.c` | `-fnoauto fun` | `const`-qualified parameter and local definitions |
| 21 | `ycf_stack_alloc.c` | `-fnoauto fun -fnoauto sub_fun` | `YCF_STACK_ALLOC(N)` (incl. the `ycf_stack_alloc_data`/size params of `_ycf_gen_yielding`) |
| 22 | `declarations_in_for_loops.c` | `-f fun` | `-f` auto-yield with declarations inside `for` headers/bodies |
| 23 | `in_code_var_declaration.c` | `-fnoauto fun` | C99 mid-block variable declaration after statements |
| 24 | `static_inline_function.c` | `-fnoauto fun` | `static inline` yieldable function |
| 25 | `yielding_fun_in_control.c` | `-fnoauto fun -fnoauto sub_fun` | Yielding fun called from `if`/`else if`/`while`/`do-while` conditions |
| 26 | `debug_ptr_to_stack.c` | `-fnoauto fun` | Defines the `-debug` hooks `ycf_debug_get_stack_start()`/`ycf_debug_global_stack_top_ptr` (see below) |
| 27 | `simple_yield_no_reds.c` | `-fnoauto fun` | `YCF_YIELD_NO_REDS()`; also asserts in-program that the reduction counter is unchanged |
| 28 | `thread_example.c` | `-f f_1 -f f_2` | Simulated cooperative threads (t1/t2/t3, fixed reduction budgets 1/2/1000) using `_ycf_gen_yielding` + `_ycf_gen_continue`; the tutorial sample |

### Examples driven by their own dedicated (non-loop) steps in `../test.sh`
| File(s) | How it is exercised |
|---------|---------------------|
| `test_generated_header_file_code.c` + `test_generated_header_file_main.c` (+`.c.out`) | Code file transformed with `-header_file_name tmp_dir/tmp.h -output_file_name tmp_dir/tmp.c -fnoauto A -fnoauto B`; main file compiled with `-I<test dir> -DYCF_YIELD_CODE_GENERATED=1` and **hardcodes `#include "tmp_dir/tmp.h"`**, linked against the generated object |
| `test_only_output_yielding_funs.c` (+`.c.out`) | Transformed with `-static_aux_funs -only_yielding_funs -output_file_name tmp_dir/tmp.ycf.h`; compiles the **original** `.c` with `-I<test dir>/tmp_dir` and **hardcodes `#include "tmp.ycf.h"`** |
| `test_only_output_yielding_funs_ptr_to_stack.c` (no `.out`) | Same `-only_yielding_funs` + `-debug`; the program must exit non-zero |
| `ycf_cannot_transform_fun_call.c` (no `.out`) | Negative test: `yielding_c_fun -yield -f fun -f sub_fun2 -f sub_fun` must exit non-zero (untransformable call to a yieldable fun, e.g. inside a larger expression) |

## Constraints
- **`.c.out` files are byte-exact expected program stdout** compared with `cmp`; a trailing newline or a change in a `printf` breaks the test. Regenerate from a real run, never hand-edit.
- **Determinism is mandatory**: no time, addresses, uninitialised data, or real thread/IPC scheduling may reach stdout (that is why `thread_example.c` simulates "threads" sequentially instead of using pthreads).
- **Positional pairing**: `../test.sh` keeps `SIMPLE_TEST_FILES` and `SIMPLE_TEST_FILES_YIELD_FUNS` as two parallel bash arrays — index *i* of one must describe index *i* of the other (currently 28 entries each).
- **Source formatting is pinned**: every file listed in `SIMPLE_TEST_FILES` is first run through `yielding_c_fun -repeat <file>` and `cmp`-ed against itself, so the file must survive parse+re-print byte-identically (no tabs/odd spacing/inconsistent indentation).
- Compilation is `clang -std=c99 -pedantic -Wall -g`, so examples must be valid C99 and warning-clean; when compiled through the normal `#else` path they must also link/run standalone.
- Hardcoded generated-file names (`tmp_dir/tmp.h`, `tmp.ycf.h`) match the paths used in `../test.sh`; renaming them in a `.c` file silently breaks the -I resolution.
- Do not add per-example build files: the corpus is only ever driven by `../test.sh`.
- One example per distinct YCF feature/edge case — prefer adding a new small file over growing an existing one.
- The `sha256_erlang_nif/` subtree is a self-contained NIF packaging example with its own build; it does not participate in `../test.sh` (the invocation is commented out) — do not touch it when editing the corpus.

## Design Decisions
- **Program stdout as the golden artifact** (instead of snapshots of the generated C): the generated code is huge, formatting-sensitive and implementation-detail-heavy; printing observable behaviour keeps goldens small, readable and robust to transform-internal changes.
- **Self-contained single-file examples**: each file is `main()` + the function under test, so a reader can copy it out, compile it with clang and run the transform on it directly. Executable behaviour (assertions on the returned value, `return 1` on failure) is embedded in `main()` in addition to the `.out` comparison.
- **`#ifdef YCF_YIELD_CODE_GENERATED` dual mode** lets the same file serve as documentation of "what the original code looked like" and as the test driver for the rewritten code.
- One file per feature keeps failures diagnosable (`cmp` names exactly one file) and gives each feature a canonical reference implementation.

## Known Issues / Gotchas
- Easy misreading: a `*.c.out` is **program stdout**, not generated/expected C source. `simple_yield.c.out` is literally `TRAPPED\nRETURNED 3\n`.
- Files without a `.c.out`: `test_generated_header_file_code.c` (its test uses `test_generated_header_file_main.c.out`), `test_only_output_yielding_funs_ptr_to_stack.c`, `ycf_cannot_transform_fun_call.c` (negative test). Absence of a `.out` does not mean "untested" for the first three.
- `debug_ptr_to_stack.c` is used twice with different expectations: the golden loop runs it **without** `-debug` (expects `TRAPPED\nRETURNED 1\n`), and a separate step runs it **with** `-debug -fnoauto fun` and requires a non-zero exit (the generated code detects the pointer-to-stack via `ycf_debug_get_stack_start()`). The same file must therefore define those `ycf_debug_*` symbols.
- YCF's `-debug` mode only works on programs that provide `ycf_debug_get_stack_start()`/`ycf_debug_global_stack_top_ptr`; `multi_scope_yield.c` is also run with `-debug` (only to check that `-log_max_mem_usage` produces a file).
- Some files here also define `YCF_CONSUME_REDS(X)`/`YCF_YIELD()` although only one of them is relevant to the flags used — harmless, the transform only reacts to the macros it is asked about.
- Older files (`simple_yield.c`, `const_defenition.c`, `in_code_var_declaration.c`, `simple_yield_no_reds.c`, …) carry no SPDX copyright header while newer ones do; both styles are accepted.

## Notes for Agents
- To add a new golden example:
  1. Create `examples/<name>.c` following the anatomy above (must survive `-repeat` round-trip byte-identically, be C99 + warning-clean).
  2. Append the absolute path (`$DIR/examples/<name>.c`) to `SIMPLE_TEST_FILES` **and** the matching flag string to `SIMPLE_TEST_FILES_YIELD_FUNS` at the *same index* in `../test.sh`.
  3. Generate `examples/<name>.c.out` by actually running the transformed program (never by editing by hand); note `../test.sh` runs `set -x`/`set -e`, so a mismatch fails loudly.
- Run the whole corpus with `make test` in `..` (target `test: $(YCF_EXECUTABLE)` → `./test/test.sh`); there is no per-example runner. `test.sh` builds into `../tmp_dir/` and expects the tool in `../bin` (and, for two steps, a `yielding_c_fun.bin` name).
- Related corpus-wide knobs live in `../GNUmakefile` (`test_add_san`, `test_mem_san`, `test_modern_cc`, `test_gcc_clang_tcc`): they re-run the same corpus with sanitizers/other compilers, so examples must be sanitizer-clean too.

## Dependencies
- `clang` (hardcoded `CC=clang` in `../test.sh`), bash, `cmp`, and a built `yielding_c_fun` (plus `yielding_c_fun.bin`) in `../bin`.

## Test Strategy
- Only exercised via `../test.sh` (driven by `make test` in `..`): repeat/round-trip check → `-yield` + compile + run + `cmp` for the 28 golden files → the generated-header-file pair → `-only_yielding_funs` pair → two `-debug` pointer-to-stack checks → the negative `ycf_cannot_transform_fun_call.c` check → `-log_max_mem_usage` file-existence check.
- No per-example runner and no Common Test suite: to iterate on one example, invoke `yielding_c_fun` and clang by hand on that file.

## Routing Table
- `sha256_erlang_nif/` → self-contained Erlang NIF packaging example (rebar3-based, vendored sha-2); has its own CONTEXT.md — read-only from here
- `..` (`..//README.md`, `../doc/thread_tutorial.md`, `../GNUmakefile`, `../test.sh`) → YCF tool itself, its CLI docs and the corpus harness (read-only; escalate edits to the parent)
