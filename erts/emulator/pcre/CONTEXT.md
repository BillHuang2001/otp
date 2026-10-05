# erts/emulator/pcre/ — vendored PCRE2 (third-party, DO NOT edit by hand)

## Intent
Statically linked, stripped-down copy of PCRE2 (Perl-Compatible Regular Expressions) that implements the
`re` module. It is upstream PCRE2 sources plus OTP integration patches plus our own build glue — not in-tree
Erlang/OTP code. 25 `.c` files (+ headers), UTF-8/8-bit only: no JIT, no DFA, no 16/32-bit, no `pcre2test`/`pcre2grep`.

## Status
- Version `10.48`; licence `BSD-3-Clause WITH PCRE2-exception` (`LICENCE`; `vendor.info` declares plain `BSD-3-Clause`).
- Upstream `https://github.com/PCRE2Project/pcre2` @ sha `7978954dbd2efc6f2196869290553cf1871b4ce6`; OTP patches live in the fork `https://github.com/erlang/pcre2`, branch `ERLANG_INTEGRATION` (must be kept in sync with this dir).
- `vendor.info` `annotation: "Vendor package modified in Erlang/OTP"`; it `exclude`s `vendor.info`, `README.pcre_update.md`, `pcre.mk`.

## OTP modifications (what "modified" means)
1. `pcre2_match.c` is patched so matching can be interrupted and restarted: reductions are consumed via `COST(n)`/`COST_CHK(n)`, the recursion stack and all state live in a heap-allocated `PcreExecContext`/`heapframe` instead of the C stack (`pcre2_free_restart_data` frees it from the BIF). Every loop in `match()` is tagged `/* LOOP_COUNT: Ok|COST|CHK */`.
2. Same integration is spread over `pcre2.h`, `pcre2_internal.h`, `pcre2_intmodedep.h`, `pcre2_context.c`, `pcre2_valid_utf.c`, all guarded by `#ifdef ERLANG_INTEGRATION`.
3. `pcre.mk` compiles with `-DERLANG_INTEGRATION` (and drops `-Wimplicit-fallthrough`); the global `-fvisibility=hidden` (set by `erts/configure.ac`) keeps PCRE2 symbols from clashing with NIFs/drivers — no `erts_` prefixing is used.
4. Irrelevant functionality stripped from the tree; build glue is entirely ours.

## OTP-side files an upstream update must NOT overwrite
- `pcre.mk` — object list, `PCRE_OBJDIR`, generated-header rules, `static_depend` target.
- `depend.mk` — committed make dependencies (`make -f pcre.mk static_depend`, gcc/Linux only, then `git add depend.mk`).
- `local_config.h` — OTP's `config.h` equivalent (`PCRE2_CODE_UNIT_WIDTH 8`, empty `PCRE2_EXPORT`, `SUPPORT_UNICODE`, LINK_SIZE/MATCH_LIMIT/HEAP_LIMIT …); refresh values from upstream `src/config.h`.
- `vendor.info`, `README.pcre_update.md`, `LICENCE` — OTP/REUSE/SBOM bookkeeping.

## How to update
Follow `README.pcre_update.md` (the authoritative, detailed procedure). Outline: clone the `erlang/pcre2` fork (`ERLANG_INTEGRATION`) → verify fork sources equal this dir → merge upstream tag `pcre2-<ver>` → re-port the `ERLANG_INTEGRATION` hunks into `pcre2_match.c` (`PcreExecContext` fields, COST/COST_CHK placement, `LOOP_COUNT` tags for every new loop/opcode) → copy sources here, update `pcre.mk` (file adds/removes) and `depend.mk`, `local_config.h` → import upstream `testoutput[1,2,4,5,10]` into `lib/stdlib/test/re_SUITE_data/` and regenerate `re_testoutput1_replacement_test`/`_split_test` via `lib/stdlib/test/run_pcre_tests.erl` → update `lib/stdlib/doc/src/re.md` (diff upstream `pcrepattern` man page) and `LICENCE`/`system/COPYRIGHT` → bump `vendor.info` `versionInfo` and add a note in `erts/doc/notes.md` → run `re_SUITE` (`make stdlib_test ARGS="-suite re_SUITE"`).

## Build integration
- Included by `erts/emulator/Makefile.in` via `include pcre/pcre.mk`; archive `$(LIB_PREFIX)epcre$(LIB_SUFFIX)` in `pcre/obj/$(TARGET)/$(TYPE)` (`PCRE_OBJDIR`, listed in `CREATE_DIRS`), linked into the emulator (`-lepcre` / `$(EPCRE_LIB)` in `LIBS`/`DEPLIBS`) and cleaned by `clean`.
- `COMMON_INCLUDES += -Ipcre`; sole consumer is `erts/emulator/beam/erl_bif_re.c` (`#define PCRE2_STATIC`, includes `../pcre/pcre2.h`) — i.e. the `re` BIFs.
- Always built from this directory: there is **no** `--enable-builtin-pcre2` or system-libpcre2 option (unlike zlib/zstd/ryu, which `--enable-builtin-*` / `--enable-use-embedded-3pp-alternatives` control). `erlang:system_info(embedded_3pps)` reports `pcre2` as included (`beam/erl_bif_info.c`).
- Three build-time generated headers come from `pcre2_match.c`: `pcre2_match_loop_break_cases.gen.h`, `pcre2_match_memchr_break_cases.gen.h`, `pcre2_match_yield_coverage.gen.h` (git-ignored via `*.gen.h`).

## Known Issues / Notes for Agents
- `pcre2_match.c` is the heavily patched file and the one upstream constantly churns; merges conflict almost always. After any update, re-verify COST/COST_CHK placement and untagged new loops — wrong accounting affects both scheduler fairness and trap-safety.
- Build output: `pcre/obj/` and `*.gen.h` are generated; the tree is otherwise fully committed (unlike `zlib`/`zstd` which also vendor generated tables here checked in as `pcre2_tables.c`/`pcre2_ucptables_inc.h`).
- Renovate (`renovate.json5`) watches `erts/emulator/pcre/vendor.info`; its pcre2-specific custom manager expects a `pcre2-<version>`-prefixed `versionInfo`, while this file currently holds a bare `10.48`.
- An update is not complete until `re_SUITE` (`pcre2`, `pcre2_incompat` groups) and the replace/split expected outputs pass.

## See Also
- `../../beam/erl_bif_re.c` → the only C consumer (BIFs behind `re`); `../../beam/erl_bif_info.c` → `embedded_3pps` list
- `lib/stdlib/doc/src/re.md` → user documentation derived from the upstream PCRE2 pattern docs
- `lib/stdlib/test/re_SUITE.erl`, `lib/stdlib/test/re_SUITE_data/`, `lib/stdlib/test/run_pcre_tests.erl` → PCRE2 conformance tests/outputs
- `../zlib/`, `../zstd/`, `../ryu/`, `../asmjit/` → sibling vendored 3PPs (each with its own `vendor.info`)
- `../../../HOWTO/SBOM.md` → vendor-package/`vendor.info` conventions
