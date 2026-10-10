# compiler/scripts — Compiler Smoke Test & Host-Side Test Rig

## Intent
Two *manual*, **test-only** developer tools. `scripts/` is not in `lib/compiler/Makefile`'s `SUB_DIRECTORIES`, so nothing here is built, installed, or run by CI:

- `smoke` — release-time end-to-end smoke test: prove the freshly built compiler can build large real-world Erlang projects (Elixir itself plus a set of Mix/hex dependencies) without crashing.
- `host_env.sh` + `host_env_xform.py` — the **host-side test rig**: build and run the in-tree `lib/compiler` application on *this machine's* Erlang runtime, without building the OTP-30 emulator. Needed because `erl`/`erlc`/`escript` on PATH are usually an older release (here: OTP 29.1.1 / erts-17.1), which (a) cannot load this tree's OTP-30 beams (`bootstrap/lib/**`: *"This BEAM file was compiled for a later version of the runtime system ... Use of opcode 193; this emulator supports only up to 191"*) and (b) resolves `-import_record` against its own installed `compiler` application, so a plain `erlc` of this tree's sources produces a compiler that crashes in its own SSA/records with `{badrecord,...}`/`function_clause`. Building the OTP-30 emulator needs `autoconf`/`m4` and is out of scope (see `otp30_env.sh` for that route).

## API Surface

### `host_env.sh` — host-side rig driver
`lib/compiler/scripts/host_env.sh {build|shell [Args...]|cleandir}`, runnable from any directory; reads the repository only, writes only inside the scratch dir.
- `build` — scratch dir `/tmp/host_env_<sha1(abspath $ERL_TOP)[0:10]>` (override with `HOST_ENV_DIR`); then 1) `host_env_xform.py` transforms `lib/compiler/src` into `$DIR/tsrc`, 2) `perl erts/emulator/utils/beam_makeops -compiler -outdir $DIR/egen lib/compiler/src/genop.tab` generates `beam_opcodes.{erl,hrl}` and the **host** `erlc` converts `core_parse.yrl` → `core_parse.erl` (both are compiled as part of the app and are copied into `tsrc`), 3) host `erlc -o $DIR/ebin -I $DIR/tsrc -I lib/stdlib/include +inline -DCOMPILER_VSN='"<COMPILER_VSN from vsn.mk>"' $DIR/tsrc/*.erl` compiles all 66 modules, 4) a probe module is compiled and run with `[loop_opt, loop_arith_licm, fusion_opt, fbip_opt]` as a self-check. Idempotent; finishes by printing `RIG_EBIN=<dir>`.
- `shell [Args...]` — `exec erl -pa $RIG_EBIN Args...`, i.e. the rig compiler first on the code path (`code:which(compile)` then reports `$RIG_EBIN/compile.beam`, version from `vsn.mk`).
- `cleandir` — `rm -rf $HOST_ENV_DIR`.
- Requires: host `erlc`, `perl`, `python3`, `sha1sum`. Never modifies repository sources.

### `host_env_xform.py SRC_DIR OUT_DIR` — the source transform
Rewrites a scratch copy of `lib/compiler/src` (`*.erl` + `*.hrl`) so the *older host erlc* can compile it. Four rewrites; anything it does not recognize makes it **exit non-zero with a message** rather than emit a silently wrong tree:
1. `-record #Name {..}` (OTP-30 namespaced/"native" records) → classic `-record(Name, {..})` with the `::` type annotations dropped.
2. `-import_record(Mod, [R1,..]).` → `-include("imp_<file>_<Mod>_<n>.hrl").` with a generated header holding exactly the converted definitions of `R1,..` taken from `Mod`'s own source. Per-import-site filtering is essential (e.g. `beam_core_to_ssa` defines its own `ifun` while `core_parse.hrl` pulls the `c_*` records from `v3_core`), and it is what makes the result independent of the host's installed `compiler` app.
3. `-export_record([...]).` → a comment.
4. The two constructs whose *semantics* depend on the native-record term type (a native record is a distinct term type in OTP ≥ 29, **not** a tuple):
   - the `#_{anno=V}` wildcard (4 sites): a clause head `f(#_{anno=V}) ->` becomes `f(XformAnnoRec) -> V = xform_anno_get(XformAnnoRec),` and `Rec#_{anno=V}` becomes `xform_anno_set(Rec, V)`; the two generated helpers dispatch on `element(1, T)` over the `anno`-carrying record tags, using the per-record `anno` element position (it is not always the first field: `#a` has it third, `#asm_module` seventh). An unknown tag raises `{not_an_annotated_record, Tag}`.
   - arity-1 `is_record/1`, which OTP ≥ 29 redefined to test for *native* records (hence `false` for every flattened tuple record). Each of the 4 sites is listed explicitly in `IS_RECORD_1`: `set_anno/2`/`set_ann/2` guards become `is_tuple/1` (their argument is always one of the former native records), and the two `beam_ssa_check:env_post/3` clauses that would call the native-record-only `records:get_name/1` become unreachable (`when false`) so the following tuple clauses handle the value.

### `smoke` — end-to-end smoke test escript
Executable escript (`#!/usr/bin/env escript`, `-mode(compile)`); `main/1` runs `setup → clone_elixir → build_elixir → test_elixir → setup_mix → smoke(main) → smoke(rabbitmq)`.
- `scripts_dir/0` locates this directory via `code:lib_dir(compiler) ++ "/scripts"`, so the escript must be run with an Erlang whose `compiler` lib dir is this source tree (put `$ERL_TOP/bin` first on `PATH`).
- `run/2` spawns commands through `open_port({spawn_executable,…})`, echoes output and aborts (`halt(1)`) on any non-zero exit code — the first failing step terminates the run.

### `smoke-mix.exs` — Mix project definition
Copied verbatim into the scratch dir as `mix.exs`. `deps/0` selects the dependency list from `SMOKE_DEPS_SET` (`main` = ~35 hex/git packages incl. cloudi, cowboy, ecto, phoenix, lager, rabbit; `rabbitmq` = `rabbit_common`; default = no deps). Custom `compile:` hooks `build_scalaris/0` and `build_wings/0` stub out native-code builds in those packages.

### `smoke-build/` — smoke-test scratch dir
Created if missing, used as cwd for all `smoke` steps; holds the `elixir/` clone, generated `mix.exs`, `deps/`, `_build/`. Only `smoke-build/mix.lock` is tracked (see `.gitignore`).

## How It Is Invoked

### Host-side rig (the common case)
```sh
lib/compiler/scripts/host_env.sh build          # -> RIG_EBIN=/tmp/host_env_<sha>/ebin
erl -noshell -pa "$RIG_EBIN" -eval '
    R = compile:file("m.erl",
                     [{i,"lib/stdlib/include"},{i,"lib/kernel/include"},
                      return,binary,loop_opt,loop_arith_licm,fusion_opt,fbip_opt]),
    {ok,_M,Bin} = {R,_,_}, ...'                 # or match element/2,3 of a 3- or 4-tuple
lib/compiler/scripts/host_env.sh shell          # interactive, rig compiler first
lib/compiler/scripts/host_env.sh cleandir
```
- Pass the `-i`/`-I` include dirs the module under test needs, otherwise compilation of that module fails with `{epp,{include,file,…}}`/`{epp,enoent}`; this is an epp path issue, not a rig fault.
- `escript` does **not** accept `-pa` on its command line (`bad option: -pa`): use `erl -noshell -pa … -eval …` or an escript whose first `%%!` line is `%%! -pa $RIG_EBIN`.
- All 66 `lib/compiler` modules compile, and the resulting compiler behaves: with `[loop_opt, loop_arith_licm, fusion_opt, fbip_opt]` it compiles and runs a probe (`f([1,2,3]) = 12`) and 96/96 `lib/stdlib/src/*.erl` compile cleanly (given stdlib/kernel includes); 27 of those 96 get a different `Code` chunk than the default path, incl. `timer.erl` — i.e. the opt-in Stage-2 SSA passes really do fire through the rig.

### `smoke`
Run the escript directly (`lib/compiler/scripts/smoke`); it takes no arguments and uses the current environment/`PATH`. Steps: create/enter `smoke-build/`, `git clone git@github.com:elixir-lang/elixir.git` (or `git pull --ff-only origin main` + `make clean` if the clone exists and HEAD moved), `make compile`, `make test_stdlib` in Elixir, install `local.hex`/`local.rebar` with the freshly built Elixir's `bin/` first on `PATH`, then per dependency set export `SMOKE_DEPS_SET`, delete `mix.lock`, `touch mix.exs`, `mix deps.clean --all`, `mix deps.get`, `mix deps.compile`. Only deps are fetched and compiled; Mix projects' own test suites are never run.

## Dependencies
- Host-side rig: `erlc`/`erl` of any older OTP on PATH, `perl` (for `beam_makeops`), `python3`, `sha1sum`; writes only to `/tmp/host_env_*`.
- `smoke`: network access (GitHub over SSH — needs a GitHub SSH key — plus hex.pm), a working `git`, `make` and a C toolchain; `mix`/`elixir` are built from the clone, not preinstalled. To be meaningful, run with the just-built OTP (`$ERL_TOP/bin` first on `PATH`).

## Known Issues
- **The host-side rig exercises the compiler's *logic*, not its record mechanism.** `host_env_xform.py` drops all record type annotations and flattens the module-namespaced/native records (`-record #Name`, `-import_record`, `-export_record`, `#_{anno=…}`, `#rec.field`) to classic tuples — in the scratch copy only. Any future source change that adds an unhandled wildcard/`is_record/1`/`import_record` site makes the transform exit non-zero (by design) until the rule table is extended. All modules are converted consistently, so the built compiler is self-consistent and its output runs on the host.
- **The compiler under test is compiled by the *host* compiler** (OTP 29 here), so this rig can never validate anything the host OTP cannot express, and it tests the *tree's* pass pipeline, not the tree's own bootstrap path.
- On this machine the host's `beam_lib:chunks/2` cannot read the `code` chunk of *any* beam — host-compiled ones included (`{error,beam_lib,{unknown_chunk,…}}`) — so comparing compiler output requires extracting the `Code` chunk from the `FOR1` container by hand (4-byte id + 32-bit size + 4-byte-aligned payload) or just running the compiled module. `beam_lib` for the rig's own use is therefore not available.
- The rig does not build an emulator, kernel/stdlib, or Common Test; run `lib/compiler/scripts/otp30_env.sh` for that (needs `autoconf`/`m4`).
- `smoke/1` deletes `mix.lock` before `mix deps.get`, so the committed `smoke-build/mix.lock` does not pin versions during a run — deps are re-resolved freshly (and the checked-in lock is overwritten in the working tree).
- `smoke` step `clone_elixir/0` runs against Elixir `main`, so the smoke test picks up Elixir changes and can fail for reasons unrelated to the Erlang compiler.
- The Elixir deps set (`main`) is pinned to old version requirements, so it can rot as hex packages are yanked/updated.

## Notes for Agents
- To test a change in `lib/compiler/src` on a machine without an OTP-30 runtime: `lib/compiler/scripts/host_env.sh build`, then drive the resulting compiler via `erl -noshell -pa $RIG_EBIN`(or `host_env.sh shell`). Re-run `build` after every source edit — it always re-transforms and recompiles from scratch, and it never touches the repository.
- `smoke-mix.exs` is matched by the repo-root `.formatter.exs` (`**/*.exs`), so it must stay `mix format`-clean or the tree-wide format check fails.
- Nothing in CI, `Makefile.in`, or `lib/compiler/Makefile` invokes `smoke` or `host_env.sh`; the `.github/scripts/otp-compliance.es` reference to `smoke-build` only excludes that directory from `.app.src` scanning.
- `HOWTO/INSTALL.md`'s "smoke test" (`erl -s ts install -s ts smoke_test batch`) is a different mechanism (test_server subset), not `smoke`.

## See Also
- `../CONTEXT.md` — the `compiler` application (build, sources, Common Test suites, the opt-in Stage-2 passes).
- `../test/` — the Common Test suites that are the normal regression coverage.
- `../otp30_env.sh` (this dir) — the full-tree route that builds the OTP-30 emulator first (needs `autoconf`/`m4`).
