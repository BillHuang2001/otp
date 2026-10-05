# Common Test — Property-Testing Support Modules

## Intent
Small support modules that `ct_property_test` uses to generate "safe" values and to paper over differences between the property-testing tools (Quviq QuickCheck / PropEr / Triq).
They are **not** part of the `common_test` application (not in `../src/Makefile` `MODULES` nor in `common_test.app.src`); `Makefile` here sets `EBIN=.` and `release_spec` installs the sources, beams and headers into `$(RELEASE_PATH)/lib/common_test-<VSN>/proper_ext`, from where `ct_property_test` adds the dir to the code path.

## API Surface
- `ct_proper_ext.erl` (`-moduledoc false`) — PropEr generators: `safe_any/0`, `safe_atom/0`, `safe_list/0`, `safe_map/0`, `safe_term/0`, `safe_tuple/0`, `existing_atom/0`. These avoid generating values that would break the test framework itself (e.g. PIDs/refs/empty atoms) and reuse the tool's own generators where possible.
- `ct_quickcheck_ext.erl` (`-moduledoc false`) — the same `safe_*`/`existing_atom/0` API for Quviq QuickCheck, plus `function2/1`, `function3/1`, `function4/1` (QuickCheck does not provide these `functionN` generators out of the box).
- `Makefile` — compiles both modules in place with `-I../include -Werror`; `release_spec` copies `*.erl`, `*.beam` and headers to `$(PROPEREXTDIR)`.

## Constraints
- Compiled with `-Werror`; must stay warning-free.
- Both modules depend on the external property-testing tool being loaded by the caller — they contain no fallback implementation.
- Do not move these into `../src/`: keeping them out of the app keeps `common_test` free of a hard dependency on any property-testing tool.

## Notes for Agents
- The macros that select between these two modules live in `../include/ct_property_test.hrl` (compile-time `EQC` / `PROPER` / `TRIQ` defines).
- The user-facing entry point is `ct_property_test` in `../src/`; the `test/ct_property_test_SUITE.erl` covers it.

## See Also
- `../src/ct_property_test.erl` — the CT API that uses these modules (read-only sibling).
- `../include/ct_property_test.hrl` — macro layer selecting the tool (read-only sibling).
- `../doc/guides/ct_property_test_chapter.md` — user documentation (read-only sibling).

## Routing Table
Flat directory, no subdirectories.
- `ct_proper_ext.erl` → PropEr `safe_*` generators
- `ct_quickcheck_ext.erl` → Quviq QuickCheck `safe_*` generators + `functionN/1`
- `Makefile` → in-place compile + release into `lib/common_test-<vsn>/proper_ext`
