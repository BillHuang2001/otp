# syntax_tools/include — Public Headers

## Intent
Public include directory of the `syntax_tools` OTP application (app version 4.1.1, `../vsn.mk`).
Contains exactly one file, `merl.hrl`, the Merl metaprogramming header; it is the only header this app installs for external consumers.
Installed to `$(RELEASE_PATH)/lib/syntax_tools-$(VSN)/include` by `../src/Makefile` (`INCLUDE_FILES = merl.hrl`, `INCLUDE_DELIVERABLES`), so it is part of the release, not an internal header.
Because it is installed, it is an external API surface: OTP apps outside `syntax_tools` (e.g. `erts/emulator/test/beam_SUITE.erl`, `small_SUITE.erl`) include it via `-include_lib`.

## API Surface
Single file `merl.hrl`, guarded by `-ifndef(MERL_HRL)` so repeated inclusion is a no-op.
Two public macros, both in call/expression position:
- `?Q(Text)` — expands to `merl:quote(?LINE, Text)`; quotes the Erlang source `Text` (a string or list of strings) into an `erl_syntax` AST / template.
- `?Q(Text, Env)` — expands to `merl:qquote(?LINE, Text, Env)`; quasi-quotes `Text` substituting the metavariable bindings in `Env`.
Side effect unless `MERL_NO_TRANSFORM` is defined: `-compile({parse_transform, merl_transform}).` — merely including the header switches on the Merl parse transform (and thereby the `_@var` metavariable / `?Q`-in-pattern features) for the including module.
The `MERL_NO_TRANSFORM` check is a plain `-ifndef`, so the macro must be defined *before* the include to take effect.
License notice at the top (SPDX `Apache-2.0 OR LGPL-2.1-or-later`) carries an explicit statement that object files using these macros are not derived works under the LGPL and the macros count as "small" regardless of line count — this note is the reason the header can be included from permissively or LGPL-licensed code and must not be removed.

## Constraints
Changes here recompile and semantically alter every module that includes the header (the macro definitions and the implicit `parse_transform` compile option both apply at the inclusion site).
Must stay compilable both with and without `MERL_NO_TRANSFORM` defined — `merl_transform.erl` itself is compiled with `-DMERL_NO_TRANSFORM` (see `../src/Makefile`), so the header cannot reference transform-only features in a way that breaks the untransformed path.
Keep the `-ifndef(MERL_NO_TRANSFORM)` guard and the `MERL_HRL` include guard: they are relied on by the bootstrap build order in `../src/Makefile` and by `../src/merl_tests.erl`.
Do not add `-record`/types that would couple this header to internal modules; it is an installed public header.
`?Q` is a deliberately short, globally visible macro name — new macros must not collide with common user-module macro names.

## Known Issues
Including the header (transformed path) makes compilation depend on the compiler node having `merl.beam` *and* `merl_transform.beam` loadable: `merl_transform:post/1` partially evaluates `?Q` at compile time via `try apply(merl, F, As)` and only catches `throw`, so an unloadable `merl` surfaces as an `undef` compile error.
That compile-time dependency is why `../src/Makefile` adds `-pa $(EBIN) -pa ./` and builds `merl.beam` before `merl_transform.beam`, and why `../examples/merl/Makefile` lists `$(EBIN)/merl_transform.beam $(EBIN)/merl.beam` as prerequisites.
`?LINE` is captured from the *inclusion/use site*, which is what lets Merl error messages point at the user's source line of the `?Q` call; quoted text spanning lines is therefore normally passed as a list of strings.
`merl.hrl` is not resolvable by bare `-include("merl.hrl")` from `../src` or `../examples/merl` without `-I` pointing at this directory (`-I$(INCLUDE)` in `../src/Makefile`, `-I$(INCLUDES)` in the examples Makefile); external code should use `-include_lib("syntax_tools/include/merl.hrl")`.

## Notes for Agents
Nothing here is generated; the header is the source of truth and has no `.src` variant.
There is no `include/` file besides `merl.hrl`, and no test suite lives in this directory (tests are in `../test/merl_SUITE.erl`).
License-header changes here should go through `scripts/` header tooling rather than hand editing; see `FILE-HEADERS/` at the repo root for the templates.

## See Also
- `../src/merl_transform.erl` — the parse transform the header activates; also explains `MERL_NO_TRANSFORM`.
- `../src/merl.erl` — the runtime functions `?Q` expands into (`quote/2`, `qquote/3`), plus the user-facing docs for metavariable syntax.
- `../src/merl_tests.erl` — uses the local `-include("merl.hrl")` form and documents the no-transform mode.
- `../test/merl_SUITE.erl`, `../examples/merl/` — the automated and example consumers of the macros.
- `../src/Makefile` — install rules (`INCLUDE_FILES`, `INCLUDE_DELIVERABLES`) and the `-DMERL_NO_TRANSFORM` bootstrap rule.
