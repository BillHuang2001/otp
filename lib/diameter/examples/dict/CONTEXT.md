# diameter — Example Dictionaries

## Intent
Sample Diameter application dictionaries written in the `diameterc` dictionary grammar (`@id`, `@inherits`, `@avp_types`, `@messages`, `@grouped`, `@enum`, `@define`), one per RFC.
They are shipped as *reference material* for users writing their own dictionaries: the example Diameter client/server code in `../code/` does **not** use them, nor does the diameter application load them at runtime.
They exist to (a) demonstrate the dictionary language against real RFC specifications and (b) act as a compile test that the current `diameterc` can still process a broad, realistic corpus.

## API Surface
Each `*.dia` declares a Diameter application id and its inheritance chain:
- `rfc4005_nas.dia` — RFC 4005 NASREQ, Diameter application id 1, `@inherits common`.
- `rfc4004_mip.dia` — RFC 4004 Mobile IP, application id 2, `@inherits common`.
- `rfc4006_cc.dia` — RFC 4006 Credit-Control, application id 4, `@inherits common` and `@inherits rfc4005_nas Filter-Id` (selective re-use of `Filter-Id`, not a full inherit).
- `rfc4072_eap.dia` — RFC 4072 EAP, application id 5, `@inherits common` and `@inherits rfc4005_nas`.
- `rfc4590_digest.dia` — RFC 4590 Digest AVPs only; no `@id` and no `@inherits` — it is a pure AVP bundle meant to be inherited by another dictionary, not a standalone application.
- `rfc4740_sip.dia` — RFC 4740 SIP, application id 6, `@inherits common` and `@inherits rfc4590_digest`.

Build helpers for the directory:
- `GNUmakefile` — builds every dictionary with `diameterc --name <d> --prefix <d> --inherits common/diameter_gen_base_rfc6733 <d>.dia` followed by `erlc -Wall +debug_info`; targets `all` (default, produces `$(DICTS:%=%.beam)`) and `clean`; `DICTS` lists the six dictionary basenames and is the single source of truth for the build.
- `depend.sed` — sed script that turns each `.dia` into `Makefile` dependency lines of the form `<dep>.erl: <inheriting-dictionary>.beam` by reading `@inherits` lines (it drops `common` and ignores everything else); the first input line must be the dictionary's filename.
- `.gitignore` — ignores the generated `/depend.mk`.

## Constraints
- Every `.dia` in this directory must keep compiling with the current `diameterc` (`diameter_make:codec/2`) because `diameter_examples_SUITE`'s `dict` case finds *all* `*.dia` files here and compiles each of them twice (against `rfc3588_base` and `rfc6733_base`); a broken or orphaned file fails the suite.
- `@inherits` references must stay resolvable and acyclic; the suite hard-codes the ordering table `rfc4006_cc`→`rfc4005_nas`, `rfc4072_eap`→`rfc4005_nas`, `rfc4740_sip`→`rfc4590_digest` (`?INHERITS` in `../../test/diameter_examples_SUITE.erl`), so a changed inheritance graph must be reflected there.
- Adding or removing a `.dia` file requires updating `EXAMPLES` in `../../src/modules.mk` (which lists the six `.dia` files for the release `dict/` directory) and, for a real new dictionary, the `DICTS` list in `GNUmakefile`.
- Keep the dictionaries in the source tree; deleting them removes the suite's compile corpus.
- Do not edit the untracked generated outputs (`*.erl`, `*.hrl`, `*.beam`, `depend.mk`) — they are rebuilt from the `.dia` files.

## Design Decisions
- `@inherits common` in the sources is deliberately rewritten at build time to the concrete module `diameter_gen_base_rfc6733` (via `diameterc --inherits common/diameter_gen_base_rfc6733`). The `From/To` form (implemented in `diameter_dict_util:opt/2` as a *re-inherit*) means "replace the existing `inherits` entry named `From` with `To`", so the dictionary language keeps the RFC-neutral keyword `common` while the build pins an actual base dictionary (`../../src/dict/base_rfc6733.dia`). Choosing rfc6733 (rather than the older rfc3588 base) is what the example Makefile standardises on; the test suite exercises *both* bases.
- `rfc4590_digest.dia` has no `@id` because it describes only additional AVPs for the Diameter base protocol, not a new Diameter application; it is consumed exclusively through `@inherits`.
- `rfc4006_cc` uses the `@inherits rfc4005_nas Filter-Id` form to borrow a single AVP; a plain `@inherits` would pull in the whole NASREQ dictionary and its messages.
- The `depend.sed`/`depend.mk` mechanism exists because dictionary compilation is order-sensitive (an inheriting dictionary needs the inherited one compiled first) and the inheritance edges are declared inside the `.dia` files, not in the Makefile; the generated `depend.mk` is `-include`d so the ordering is derived from the sources.
- `GNUmakefile` is a standalone convenience for building the examples by hand and is not wired into the OTP build; it requires `erlc` and `diameterc` on `PATH`.

## Notes for Agents
- The output of a `.dia` is a dictionary codec module: `<d>.erl` + `<d>.hrl` record definitions + `<d>.beam`, all untracked (the parent `../.gitignore` covers `*.beam`; `.SECONDARY` keeps the `.erl` after the `.beam` rule runs).
- To compile one by hand: `diameterc --name rfc4005_nas --prefix rfc4005_nas --inherits common/diameter_gen_base_rfc6733 rfc4005_nas.dia` (add a second `--inherits` for the dictionaries that inherit from a sibling here), then `erlc -Wall +debug_info rfc4005_nas.erl`.
- `diameterc --name` sets the generated module name, `--prefix` the prefix of the generated record/constant names — both are set to the dictionary basename here.
- The `common` keyword resolves to the dictionary bundled with the diameter application (the `<app>_base` names are produced from `../../src/dict/base_rfc3588.dia` / `base_rfc6733.dia`); `@inherits common` therefore needs a dictionary named `common` in the inheritance path.
- Only the six `.dia` files are released into `doc/examples/dict/`; `GNUmakefile`, `depend.sed` and `.gitignore` stay in the source tree but are still present in the test release because `../../test/Makefile` tars the whole `examples/` tree.
- Dictionary grammar and `diameterc` options are documented in `../../doc/references/diameter_dict.md` (`diameter_dict(4)`) and `../../doc/references/diameterc_cmd.md` (`diameterc(1)`); `diameterc --help` lists the options, and the grammar implementation lives in `../../src/compiler/`.

## Test Strategy
- Covered by the `dict` testcase in `../../test/diameter_examples_SUITE.erl` (run with the `diameter` Common Test suites): it locates this directory relative to the suite module, creates a temp output dir and compiles every `*.dia` with both `rfc3588_base` and `rfc6733_base` as the base dictionary, in dependency order (sorted by the `?INHERITS` table plus a recursive `dep/1` walk), asserting every `make` and `compile` returns ok.
- The testcase is skipped/deferred when the machine is considered too slow, so a green `diameter_examples_SUITE` run is the authoritative signal for changes here; there is no unit-level test of individual dictionaries.
- `make` in this directory (using `GNUmakefile`) is the quick manual smoke test, but it only pins the rfc6733 base while the suite covers both.

## See Also
- `../` — other diameter examples; `../code/` — example Diameter client/server code (independent of these dictionaries).
- `../../src/compiler/` — `diameterc` / dictionary-grammar implementation (`diameter_dict_util.erl` for `@inherits` semantics).
- `../../src/dict/` — the `diameter` application's own bundled dictionaries, including the `*_base` dictionaries inherited here.
- `../../test/diameter_examples_SUITE.erl` — the contract for these files.
