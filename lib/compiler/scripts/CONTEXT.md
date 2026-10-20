# compiler/scripts — Compiler Smoke Test, Host-Side Test Rig & A.9 Self-Host Probe

## Intent
Three *manual*, **test-only** developer tools. `scripts/` is not in `lib/compiler/Makefile`'s `SUB_DIRECTORIES`, so nothing here is built, installed, or run by CI:

- `smoke` — release-time end-to-end smoke test: prove the freshly built compiler can build large real-world Erlang projects (Elixir itself plus a set of Mix/hex dependencies) without crashing.
- `host_env.sh` + `host_env_xform.py` — the **host-side test rig**: build and run the in-tree `lib/compiler` application on *this machine's* Erlang runtime, without building the OTP-30 emulator. Needed because `erl`/`erlc`/`escript` on PATH are usually an older release (here: OTP 29.1.1 / erts-17.1), which (a) cannot load this tree's OTP-30 beams (`bootstrap/lib/**`: *"This BEAM file was compiled for a later version of the runtime system ... Use of opcode 193; this emulator supports only up to 191"*) and (b) resolves `-import_record` against its own installed `compiler` application, so a plain `erlc` of this tree's sources produces a compiler that crashes in its own SSA/records with `{badrecord,...}`/`function_clause`. Building the OTP-30 emulator needs `autoconf`/`m4` and is out of scope (see `otp30_env.sh` for that route).
- `a9_selfhost_probe.sh` (+ `A9_CASES.md`) — the **A.9 self-hosted-build probe**: measure, rung by rung, how far the Rust rewrite's `erts_rust/target/*/beam_rt` gets towards compiling this tree with the tree's *own* committed OTP-30 bootstrap compiler, and run the A.9 differential case family it defines. Read-only with respect to the repository; it never claims a self-hosted build that does not exist.

## API Surface

### `host_env.sh` — host-side rig driver
`lib/compiler/scripts/host_env.sh {build|shell [Args...]|cleandir}`, runnable from any directory; reads the repository only, writes only inside the scratch dir.
- `build` — scratch dir `/tmp/host_env_<sha1(abspath $ERL_TOP)[0:10]>` (override with `HOST_ENV_DIR`; the default is the literal `/tmp`, not `$TMPDIR` — `host_env.sh:55-57`); then 1) `host_env_xform.py` transforms `lib/compiler/src` into `$DIR/tsrc` (`host_env.sh:75`), 2) `perl erts/emulator/utils/beam_makeops -compiler -outdir $DIR/egen lib/compiler/src/genop.tab` generates `beam_opcodes.{erl,hrl}` and the **host** `erlc` converts `core_parse.yrl` → `core_parse.erl`; those three are copied into `tsrc` (`host_env.sh:79-83`), 3) host `erlc -o $DIR/ebin -I $DIR/tsrc -I lib/stdlib/include +inline -DCOMPILER_VSN='"<COMPILER_VSN from vsn.mk>"' $DIR/tsrc/*.erl` compiles all **67** modules (`host_env.sh:86-91`: the 65 in-tree `src/*.erl` + those 2 generated `.erl`; `src/Makefile:51-117` lists 67 `MODULES` and the 2 generated ones are `beam_opcodes`/`core_parse`), 4) a probe module is compiled and run with `[loop_opt, loop_arith_licm, fusion_opt, fbip_opt]` as a self-check (`host_env.sh:93-112`, expects `probe:f([1,2,3]) = 12`). Idempotent (`rm -rf $EGEN $EBIN` + the transform rmtree's `tsrc` each run); finishes by printing `RIG_EBIN=<dir>` (`host_env.sh:115`).
- Exit code / usage: an unknown or missing subcommand prints `sed -n '23,38p'` of the script (the comment header, `host_env.sh:134`) and exits 2 — that range stops at the `shell` line, so the usage message it prints does **not** list `cleandir`.
- **No host-OTP version gate.** The script only checks that `erlc` exists (`host_env.sh:51,65`), prints `erlc -version` (`:70`) and derives `COMPILER_VSN` from `lib/compiler/vsn.mk` (`:86-88`) — it never compares host versions, and `lib/compiler/test/run_stage2_suites.sh` does not either. The OTP-29-class host assumed by the docstrings (`host_env.sh:26-29`, `host_env_xform.py:24-25`) is a convention, not a check.
- `shell [Args...]` — `exec erl -pa $RIG_EBIN Args...`, i.e. the rig compiler first on the code path (`code:which(compile)` then reports `$RIG_EBIN/compile.beam`, version from `vsn.mk`).
- `cleandir` — `rm -rf $HOST_ENV_DIR`.
- Requires: host `erlc` (`host_env.sh:51,65`), `erl` (the step-4 self-check, `:100` — not pre-checked), `perl` (`:67`, for `beam_makeops`), `python3` (`:66`), `sha1sum` (`:55`, only when `HOST_ENV_DIR` is unset — under `set -e` a missing `sha1sum` aborts the script). No erlc version constraint (see the no-version-gate bullet above). Never modifies repository sources. Only the *sources* are needed: `lib/compiler/src/{*.erl,*.hrl,genop.tab,core_parse.yrl}`, `erts/emulator/utils/beam_makeops` (a perl script), `lib/stdlib/include`, `lib/compiler/vsn.mk` — no `configure`/`make`, no in-tree emulator, no built kernel/stdlib.

### `host_env_xform.py SRC_DIR OUT_DIR` — the source transform
Rewrites a scratch copy of `lib/compiler/src` (`*.erl` + `*.hrl`, 65 + 6 files today, `host_env_xform.py:220-221`) so the *older host erlc* can compile it. Four rewrites; anything it does not recognize makes it **exit non-zero with a message** rather than emit a silently wrong tree:
1. `-record #Name {..}` (OTP-30 namespaced/"native" records; 156 definitions today) → classic `-record(Name, {..})` with the `::` type annotations dropped (`RECORD_RE`:62, `record_defs`:99-123, `strip_type`:86-96, `convert_local_defs`:126-136).
2. `-import_record(Mod, [R1,..]).` (8 `.erl` + 3 `.hrl` sites) → `-include("imp_<file>_<Mod>_<n>.hrl").` with a generated header holding exactly the converted definitions of `R1,..` taken from `Mod`'s own source (`rewrite_imports`:139-158). Per-import-site filtering is essential (e.g. `beam_core_to_ssa` defines its own `ifun` while `core_parse.hrl` pulls the `c_*` records from `v3_core`), and it is what makes the result independent of the host's installed `compiler` app. All current sites import only `opt_st`/`func_info` (from `beam_ssa_opt`) and the `v3_core`/`beam_types`/`beam_ssa` record sets.
3. `-export_record([...]).` → a comment (`EXPORT_RE`:64, applied at `:238`; 4 sites: `beam_types`, `v3_core`, `beam_ssa`, `beam_ssa_opt`).
4. The two constructs whose *semantics* depend on the native-record term type (a native record is a distinct term type in OTP ≥ 29, **not** a tuple):
   - the `#_{anno=V}` wildcard (4 sites: `cerl.erl:320,332`, `v3_core.erl:4997,5002`): a clause head `f(#_{anno=V}) ->` becomes `f(XformAnnoRec) -> V = xform_anno_get(XformAnnoRec),` and `Rec#_{anno=V}` becomes `xform_anno_set(Rec, V)` (`rewrite_wildcards`:186-201, regexes `:65-66`); the two generated helpers (`helper_functions`:204-213) dispatch on `element(1, T)` over the `anno`-carrying record tags, using the per-record `anno` element position (`anno_pos`:222-228; it is not always the first field: `#a` has it third, `#asm_module` seventh). An unknown tag raises `{not_an_annotated_record, Tag}`.
   - arity-1 `is_record/1`, which OTP ≥ 29 redefined to test for *native* records (hence `false` for every flattened tuple record). Each of the 4 sites is listed explicitly in `IS_RECORD_1` (`:169-177`, applied `:240-243`): `set_anno/2` (`v3_core.erl:5001`) / `set_ann/2` (`cerl.erl:331`) guards become `is_tuple/1` (their argument is always one of the former native records), and the two `beam_ssa_check:env_post/3` clauses (`beam_ssa_check.erl:268,272`) that would call the native-record-only `records:get_name/1` become unreachable (`when false`) so the following tuple clauses handle the value.

### `smoke` — end-to-end smoke test escript
Executable escript (`#!/usr/bin/env escript`, `-mode(compile)`); `main/1` runs `setup → clone_elixir → build_elixir → test_elixir → setup_mix → smoke(main) → smoke(rabbitmq)`.
- `scripts_dir/0` locates this directory via `code:lib_dir(compiler) ++ "/scripts"`, so the escript must be run with an Erlang whose `compiler` lib dir is this source tree (put `$ERL_TOP/bin` first on `PATH`).
- `run/2` spawns commands through `open_port({spawn_executable,…})`, echoes output and aborts (`halt(1)`) on any non-zero exit code — the first failing step terminates the run.

### `smoke-mix.exs` — Mix project definition
Copied verbatim into the scratch dir as `mix.exs`. `deps/0` selects the dependency list from `SMOKE_DEPS_SET` (`main` = ~35 hex/git packages incl. cloudi, cowboy, ecto, phoenix, lager, rabbit; `rabbitmq` = `rabbit_common`; default = no deps). Custom `compile:` hooks `build_scalaris/0` and `build_wings/0` stub out native-code builds in those packages.

### `smoke-build/` — smoke-test scratch dir
Created if missing, used as cwd for all `smoke` steps; holds the `elixir/` clone, generated `mix.exs`, `deps/`, `_build/`. Only `smoke-build/mix.lock` is tracked (see `.gitignore`).

### `a9_selfhost_probe.sh` — the A.9 self-hosted-build probe
`lib/compiler/scripts/a9_selfhost_probe.sh [ladder|cases|--help]`, runnable from any directory; reads the repository, writes only under `$TMPDIR/a9_selfhost` (`A9_DIR` overrides), never inside the repo.
- `ladder` (default) — runs L1–L7 and prints **one line per rung**, `L<n> <name>: PASS|BLOCKED|FAIL — <evidence>`, then `a9_selfhost_probe: ladder: <p> PASS, <b> BLOCKED, <f> FAIL (<n> rungs, <t>)`. Exit 0 when every rung ran (a `BLOCKED` rung is a successful measurement of a rung that does not work), 1 on any `FAIL`, 2 on a harness error (no `beam_rt`, no oracle, missing source).
- `cases` — runs the nine-case A.9 family of `A9_CASES.md` against oracle + `beam_rt` and prints `<id>: agree|unsupported|mismatch|skipped — <detail>` per case; exit 1 only if a `mismatch` appears (an `unsupported` is a recorded gap, a `skipped` a recorded non-verdict).
- `BEAM_RT` overrides the runtime (default: `erts_rust/target/{release,debug}/beam_rt`, first that exists); `ERTS_ORACLE`/`ERLC` the oracle (default: PATH); `A9_COMPILER_MODULES` the L3 module set (default `sys_core_bsm beam_flatten`); `A9_L2_SRC` the L2 module (default `lib/compiler/src/cerl_clauses.erl`); `A9_VERBOSE=1` echoes each command's output.
- Rung coverage: L1 `cli_eval` (`-eval '1+1'` → `2`); L2 `in_process_compile` (one real OTP module, byte-`cmp`ed against `erlc +deterministic`); L3 `compiler_chunk` (the same over the tree's **own** `lib/compiler/src` sources — "the Rust runtime compiling the compiler"); L4 `tree_own_compiler` (the tree's committed OTP-30 bootstrap compiler on the code path — the A.9 blocker); L5 `erlc_frontend` (the real `erlc` with `ERLC_EMULATOR=<beam_rt>`, with the front end's argv captured by a logging wrapper); L6 `emake_driver` (the `make` module with the correct `{emake,[{Src,Opts}]}` in-term form); L7 `make_command_line` (the `make/otp.mk.in` compile rule).
- Every artifact comparison uses `deterministic`/`{deterministic,true}` on **both** sides (the `CInf`/`Line` chunks otherwise embed the source path and cwd), and a rung that produced nothing asserts the artifact's *absence* — it never reports PASS on a comparison of nothing.

### `A9_CASES.md` — the A.9 differential case family
The definition of the nine proposed cases (id, contract letter, oracle invocation, Rust invocation, golden assertion, verdict today) plus, for each blocked case, the exact typed error and its owner crate; also the paragraphs on what is *deliberately not* a case and on how to land the family in `erts_rust/tools/differential` (`src/corpus/selfhost.rs`, `known-gaps.txt`, `gate.sh`). Analytical/planning document — no code.

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
- All 67 `lib/compiler` modules compile, and the resulting compiler behaves: with `[loop_opt, loop_arith_licm, fusion_opt, fbip_opt]` it compiles and runs a probe (`f([1,2,3]) = 12`) and 96/96 `lib/stdlib/src/*.erl` compile cleanly (given stdlib/kernel includes); 27 of those 96 get a different `Code` chunk than the default path, incl. `timer.erl` — i.e. the opt-in Stage-2 SSA passes really do fire through the rig.

### `smoke`
Run the escript directly (`lib/compiler/scripts/smoke`); it takes no arguments and uses the current environment/`PATH`. Steps: create/enter `smoke-build/`, `git clone git@github.com:elixir-lang/elixir.git` (or `git pull --ff-only origin main` + `make clean` if the clone exists and HEAD moved), `make compile`, `make test_stdlib` in Elixir, install `local.hex`/`local.rebar` with the freshly built Elixir's `bin/` first on `PATH`, then per dependency set export `SMOKE_DEPS_SET`, delete `mix.lock`, `touch mix.exs`, `mix deps.clean --all`, `mix deps.get`, `mix deps.compile`. Only deps are fetched and compiled; Mix projects' own test suites are never run.

### A.9 probe
```sh
# build the runtime once (from the repo root; the crate cache is shared):
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt
lib/compiler/scripts/a9_selfhost_probe.sh ladder          # ~1 min, 7 rungs + summary
lib/compiler/scripts/a9_selfhost_probe.sh cases           # ~50 s, 9 case verdict lines
A9_VERBOSE=1 lib/compiler/scripts/a9_selfhost_probe.sh ladder   # echo every command
```
- An in-process compile of one module of this tree costs ~6–24 s under `beam_rt` (the oracle needs ~0.2 s), so keep `A9_COMPILER_MODULES`/`A9_L2_SRC` small; `beam_ssa_loop_cfg.erl` alone takes ~105 s and is deliberately **not** in either default set.
- `erlc` resolves `ERLC_EMULATOR` against its own cwd, so the probe always exports an absolute `beam_rt` path.

## Dependencies
- Host-side rig: `erlc`/`erl` of any older OTP on PATH, `perl` (for `beam_makeops`), `python3`, `sha1sum`; writes only to `/tmp/host_env_*`.
- `smoke`: network access (GitHub over SSH — needs a GitHub SSH key — plus hex.pm), a working `git`, `make` and a C toolchain; `mix`/`elixir` are built from the clone, not preinstalled. To be meaningful, run with the just-built OTP (`$ERL_TOP/bin` first on `PATH`).
- A.9 probe: a built `erts_rust/target/{release,debug}/beam_rt` (or `BEAM_RT`), the system `erl`/`erlc` as oracle, and the committed `bootstrap/lib/{kernel,stdlib,compiler}/ebin` for L4; `bash`, `awk`, `sed`, `cmp`, `date +%s%N`. No `configure`/`make` run, no network.

## Known Issues
- **The host-side rig exercises the compiler's *logic*, not its record mechanism.** `host_env_xform.py` drops all record type annotations and flattens the module-namespaced/native records (`-record #Name`, `-import_record`, `-export_record`, `#_{anno=…}`, `#rec.field`) to classic tuples — in the scratch copy only. Any future source change that adds an unhandled wildcard/`is_record/1`/`import_record` site makes the transform exit non-zero (by design) until the rule table is extended. All modules are converted consistently, so the built compiler is self-consistent and its output runs on the host.
- **The compiler under test is compiled by the *host* compiler** (OTP 29 here), so this rig can never validate anything the host OTP cannot express, and it tests the *tree's* pass pipeline, not the tree's own bootstrap path.
- On this machine the host's `beam_lib:chunks/2` cannot read the `code` chunk of *any* beam — host-compiled ones included (`{error,beam_lib,{unknown_chunk,…}}`) — so comparing compiler output requires extracting the `Code` chunk from the `FOR1` container by hand (4-byte id + 32-bit size + 4-byte-aligned payload) or just running the compiled module. `beam_lib` for the rig's own use is therefore not available.
- **The transform is textual and anchored, so it can miss a site instead of failing:** `-record #Name` must start at column 0 (`RECORD_RE` is used with `re.match`, `host_env_xform.py:62,105`) and comments inside a record block are dropped (`:99-102`); `-import_record(Mod,…)` resolves only when `Mod`'s *source is one of the transformed files* (`defs_by_mod` is built from `src` alone, `:229-231`) and otherwise calls `sys.exit` (`:154-155`); the clause-head wildcard rule only matches the single-argument form `f(#_{anno=V}) ->` (`:66,195`). The safety net is the explicit "unhandled …remain" checks (`:197-200`, `:244-247`) plus a hard-coded `IS_RECORD_1` text match that `sys.exit`s if the expected source line is gone (`:241-242`).
- The scratch tree has no application resource: only `*.erl`/`*.hrl` are copied (`host_env_xform.py:220-221`), never `compiler.app.src`, `genop.tab`, `core_parse.yrl` or `Makefile`, so the rig ebin is a bare directory of beams — no `.app` file, and `code:lib_dir(compiler)` under the rig does not point at it.
- The `.gitignore` here lists `/ct_ebin/` and `/ct_logs/` as `otp30_env.sh` scratch output, but that script uses `LOGDIR=$ERL_TOP/lib/compiler/scripts/ct_logs` only and compiles the suites *into* the test dir (`CTBIN="$TESTDIR"`, whose `*.beam` the global ignore already covers), so `/ct_ebin/` matches nothing today.
- The rig does not build an emulator, kernel/stdlib, or Common Test; run `lib/compiler/scripts/otp30_env.sh` for that (needs `autoconf`/`m4`).
- `smoke/1` deletes `mix.lock` before `mix deps.get`, so the committed `smoke-build/mix.lock` does not pin versions during a run — deps are re-resolved freshly (and the checked-in lock is overwritten in the working tree).
- `smoke` step `clone_elixir/0` runs against Elixir `main`, so the smoke test picks up Elixir changes and can fail for reasons unrelated to the Erlang compiler.
- The Elixir deps set (`main`) is pinned to old version requirements, so it can rot as hex packages are yanked/updated.
- **The A.9 probe measures; it does not build the tree.** Its two blocked rungs are live capability gaps in `beam_rt`, not probe faults: L4 = the tree's own OTP-30 bootstrap compiler cannot compile *any* module (`compile:file/2` returns the string `'unsupported_opcode: put_record: create: no record definition table for another module'`, a create-form `put_record` naming a foreign `{Module,Name}` record — `crates/beam_engine/src/families/records/mod.rs::local_definition`), L5 = the real `erlc` front end dies on `beam_rt: unknown flag -S1` (compact `+S1`/`+sbtu`) and `init:get_plain_arguments()` is `[]` under this runtime. L7 is environment-only: `./configure` re-execs `$CONFIG_SHELL`, which is a missing `/nix/store/…-bash-5.3p9/bin/bash`, so `make/$(TARGET)/otp.mk` never exists. Measured ladder: 4 PASS / 3 BLOCKED / 0 FAIL.
- The L4 observable is **module dependent**: `lib/compiler/src/beam_flatten.erl` yields the typed reason above, while `cerl_clauses.erl` makes the tree compiler print its own `Function: is_catchall/1` diagnostic and answer `error` (the same gap caught by `compile`'s per-function handler). A probe that pinned only the second shape would misclassify a gap as a mismatch.
- The L4 corroboration is re-measured on every ladder run: all 59/59 host OTP-29 `compiler/ebin/*.beam` carry **0** `put_record` instructions (0 create-form), because pre-OTP-30 records are flattened to `put_tuple2` at compile time — the reason a 755/755 byte-identical in-process sweep never executed the record-table path. The tree's OTP-30 bootstrap beams do carry them (583 create-form in `v3_core.beam`, 113 in `beam_ssa_opt.beam`, measured once under `beam_rt`; a full re-count costs ~2 min and is not part of the probe).
- `a9_selfhost_probe.sh cases` deliberately has **no** case for `-extra` plain arguments: the oracle prints `["a","b"]` where `beam_rt` prints `[]`, which is a `mismatch` (a differing observable, not a signalled gap) and is already recorded in `erts_rust/tools/differential/divergences.md`. The L5 rung reports the value instead, so the fact is reproduced without a red run.

## Notes for Agents
- To test a change in `lib/compiler/src` on a machine without an OTP-30 runtime: `lib/compiler/scripts/host_env.sh build`, then drive the resulting compiler via `erl -noshell -pa $RIG_EBIN`(or `host_env.sh shell`). Re-run `build` after every source edit — it always re-transforms and recompiles from scratch, and it never touches the repository.
- `smoke-mix.exs` is matched by the repo-root `.formatter.exs` (`**/*.exs`), so it must stay `mix format`-clean or the tree-wide format check fails.
- To re-check A.9 after any `beam_rt` change: `lib/compiler/scripts/a9_selfhost_probe.sh ladder` (≈1 min). L4/L5 turning `PASS` is the signal to delete the two allowlist entries in `erts_rust/tools/differential/known-gaps.txt` and tighten the ratchet — the probe's `cases` mode already byte-compares an artifact whenever one is produced, so nothing else needs editing.
- Nothing in CI, `Makefile.in`, or `lib/compiler/Makefile` invokes `smoke`, `host_env.sh` or `a9_selfhost_probe.sh`; the `.github/scripts/otp-compliance.es` reference to `smoke-build` only excludes that directory from `.app.src` scanning.
- `HOWTO/INSTALL.md`'s "smoke test" (`erl -s ts install -s ts smoke_test batch`) is a different mechanism (test_server subset), not `smoke`.

## See Also
- `../CONTEXT.md` — the `compiler` application (build, sources, Common Test suites, the opt-in Stage-2 passes).
- `../test/` — the Common Test suites that are the normal regression coverage.
- `A9_CASES.md` (this dir) — the A.9 differential case family: ids, invocations, goldens, expected verdicts, gap owners, and how to land it in `erts_rust/tools/differential`.
- `../../../erts_rust/tools/differential/` — `known-gaps.txt` (the ratchet allowlist), `divergences.md` (unpinnable divergences), `gate.sh`; read-only from this node.
- `../otp30_env.sh` (this dir) — the full-tree route that builds the OTP-30 emulator first (needs `autoconf`/`m4`).
