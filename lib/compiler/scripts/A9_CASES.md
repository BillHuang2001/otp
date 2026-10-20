# A.9 (self-hosted build) — differential case family and probe ladder

**Status:** measurement harness landed at `lib/compiler/scripts/a9_selfhost_probe.sh`; the corpus cases
below are **proposed** (`erts_rust/tools/differential` is read-only from `lib/compiler/scripts`) and
land as `src/corpus/selfhost.rs` additions by the owner of that tree.
Every observation in this file was produced by `a9_selfhost_probe.sh ladder` / `… cases` on
**2026-10-20** against the system oracle (`erl`/`erlc` on `PATH`, **OTP 29.1.1 / erts-17.1**) and
`erts_rust/target/debug/beam_rt` built from this worktree. Re-run the probe after any `beam_rt`
change: the ladder and the case family are the live re-check.

**The point of the family.** A.9 is the contract "the tree builds itself". The oracle cannot load this
tree's OTP-30 beams at all — `code:load_abs("bootstrap/lib/compiler/ebin/compile")` answers
`{error,badfile}` and the emulator logs *"This BEAM file was compiled for a later version of the
runtime system than the current (Erlang/OTP 29) … (Use of opcode 193; this emulator supports only up
to 191.)"* — so the *last* rungs of the ladder (the tree's own compiler on the tree's sources) can
only be observed on `beam_rt`, and the case family must say which cases are genuine
oracle-vs-Rust comparisons and which are recorded capability gaps.

**House conventions used here** (the `erts_rust/tools/differential` ones): an `agree` case is a
*compared* observation; an `unsupported` case is a capability gap the runtime signals through its
documented convention (a typed `unsupported…` reason, or its documented exit 1/3 refusal);
a `mismatch` is a differing observable and is always a hard failure; a `skipped` case is not a
verdict. Only `mismatch` is a failure — a recorded gap is not.

---

## 1. The ladder `a9_selfhost_probe.sh ladder`

One stdout line per rung: `L<n> <name>: PASS|BLOCKED|FAIL — <evidence>`. `BLOCKED` is a *successful
measurement* of a rung that does not work; `FAIL` means the measured outcome contradicts the rung's
contract (and makes the script exit 1). Rungs L1–L7 measure, in order: the `-eval` route, an
in-process `compile:file/2` of a real OTP module, the same over the tree's **own** compiler sources,
the tree's **committed bootstrap compiler**, the real `erlc` front end, the `make` module driver, and
the OTP build's own compile command line.

Measured ladder (host OTP 29.1.1, `debug` `beam_rt`, full run **62 s** — an in-process compile of a
tree module costs ~6–24 s here, the oracle ~0.2 s):

| rung | name | verdict | evidence (abridged) |
| --- | --- | --- | --- |
| L1 | `cli_eval` | **PASS** | `-noshell -eval 'io:format("~p~n",[1+1])'` → `2`, exit 0 (1.2 s) |
| L2 | `in_process_compile` | **PASS** | `lib/compiler/src/cerl_clauses.erl` → `{ok,cerl_clauses}`, byte-identical to `erlc +deterministic`, 18.3 s vs 0.20 s |
| L3 | `compiler_chunk` | **PASS** | `sys_core_bsm` + `beam_flatten` 2/2 byte-identical (the Core and the codegen/asm stage), 17.2 s vs 0.37 s |
| L4 | `tree_own_compiler` | **BLOCKED** | `code:which(compile)` = the tree's `bootstrap/lib/compiler/ebin/compile.beam`; `compile:file` → `'unsupported_opcode: put_record: create: no record definition table for another module'`, nothing produced |
| L5 | `erlc_frontend` | **BLOCKED** | `ERLC_EMULATOR=<beam_rt> erlc +deterministic -o …` → `beam_rt: unknown flag -S1`, exit 1, nothing produced |
| L6 | `emake_driver` | **PASS** | `make:all([{emake,[{Src,[deterministic,{outdir,…}]}]}])` → `Recompile: …` + `up_to_date` on both, artifact byte-identical (15.1 s vs 0.21 s) |
| L7 | `make_command_line` | **BLOCKED** | `make -C lib/<app>` unreachable (unconfigured tree); the `otp.mk.in` rule driven directly reaches L5's `-S1` refusal |

Summary line: `a9_selfhost_probe: ladder: 4 PASS, 3 BLOCKED, 0 FAIL (7 rungs, 62.21s)`, exit 0.

**L4 — the A.9 blocker, in full.** The rung asserts **both** facts: that the tree's own compiler beam
is what got loaded (`code:which(compile)` ends in `bootstrap/lib/compiler/ebin/compile.beam`) *and*
the exact typed reason. Root cause: a **create-form `put_record`** instruction whose `Id` operand is a
**foreign** `{Module,Name}` record reference (the tree's compiler sources carry
`-export_record([...])`/`-import_record(...)`, so record creation names another module's definition).
`erts_rust/crates/beam_engine/src/families/records/mod.rs::local_definition` (~L817–842) resolves only
the running module's own definitions and returns `error::unsupported("…: create: no record definition
table for another module")` for a foreign id; the C resolves it through the module's **record
definition table** (`erts_record_find_entry`) plus a `code:ensure_loaded/1` trap. Gap owner:
`beam_engine` (records family); seam owners for the table/trap: `beam_bifs`/`beam_rt`.

**Why a 755/755 byte-identical in-process sweep never reached that path** (L4's corroboration, which
the rung re-measures on every run): all **59/59** host OTP-29 `compiler/ebin/*.beam` carry **0**
`put_record` instructions (0 create-form) — pre-OTP-30 records are flattened to `put_tuple2` at
compile time, so a compiler *built by the host compiler* never executes a record-table lookup. The
tree's OTP-30 bootstrap compiler does: disassembling its beams under `beam_rt` counts 583 create-form
`put_record` instructions in `v3_core.beam` and 113 in `beam_ssa_opt.beam` (recursive instruction
walk; measured once during this work, not re-measured by the probe — it costs ~2 min).

**The failure's shape is module dependent.** `lib/compiler/src/beam_flatten.erl` surfaces the typed
reason above; `lib/compiler/src/cerl_clauses.erl` instead makes the tree's compiler print its own
`Function: is_catchall/1` diagnostic and answer the bare atom `error` (the same gap caught by
`compile`'s per-function error handler). The rung and case 7 therefore use `beam_flatten.erl`. A
harness that used only `cerl_clauses.erl` would have to call this a `mismatch`, which would be wrong.

**L5 — both root causes, measured.** The OTP `erlc` C front end honours `ERLC_EMULATOR`; a logging
wrapper proves the argv it hands the runtime is exactly
`[+S1, +sbtu, +A0, +P, 65536, +Q, 1024, -noinput, -mode, minimal, -boot, no_dot_erlang, -s, erl_compile, compile_cmdline, -extra, +deterministic, -o, <D>, <src>]`.
`beam_rt` accepts `+A0`, `+P N`, `+Q N`, `-mode minimal`, `-boot no_dot_erlang`, `-s M F` and the
**separated** `+S 1`, and rejects the **compact** `+S1` and `+sbtu` (`+S` parsing wants a separate
value token, `beam_rt/src/cli.rs:488`) → `beam_rt: unknown flag -S1`, exit 1, nothing produced.
Independently, `init:get_plain_arguments()` is `[]` under this runtime (`beam_rt/src/native.rs:40`,
`beam_rt/src/native/init.rs:175`), so even with the two flags fixed `erl_compile:compile_cmdline/0`
(`lib/stdlib/src/erl_compile.erl:53-56`) could not see the file list. Owner: `beam_rt`.

**L7 — environment only.** `./configure` cannot complete in this sandbox: it re-execs
`$CONFIG_SHELL`/`$SHELL`, which is `/nix/store/zh1ijdhb6gng1509b1zrilb6xlzx60j6-bash-5.3p9/bin/bash`
and **does not exist** (`./configure: line 132: … No such file or directory`), so
`make/$(TARGET)/otp.mk` is never generated (only `make/autoconf/` exists). The probe therefore drives
the *exact* command line of the build rule (`make/otp.mk.in:134-137`:
`$(V_ERLC) $(ERL_COMPILE_FLAGS) -o$(EBIN) $<`) directly, and it lands on L5's refusal. No
`./configure` is run by the probe: it would write generated files into the repository.

---

## 2. The case family `a9_selfhost_probe.sh cases`

Run today (50 s): `6 agree, 2 unsupported, 0 mismatch, 1 skipped`, exit 0. Ids are proposed in the
existing `selfhost/…` house style and do not collide with the 26 landed `selfhost/*` ids. Contract
letters are the `Contract` enum (`Build` = A.9, `BeamAsm` = A.2, `Cli` = A.6, `OtpApps` = A.7).

| # | case id | contract | oracle invocation | Rust invocation | golden assertion | verdict today |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | `selfhost/eval_route_answers_two_plus_two` | A.9/A.6 | `erl -noshell -eval 'io:format("~p~n",[1+1]),halt(0).'` | same argv, `beam_rt` in place of `erl` | both print `2`, exit 0 | **agree** |
| 2 | `selfhost/in_process_compile_of_the_compilers_own_source` | A.9/A.2 | `erlc +deterministic -o D lib/compiler/src/cerl_clauses.erl` | `compile:file("…/cerl_clauses.erl",[deterministic,{outdir,D2}])` | `{ok,cerl_clauses}` on both; `cmp` byte-identical | **agree** (14.1 s vs 0.24 s) |
| 3 | `selfhost/in_process_compile_of_the_compilers_core_stage` | A.9/A.2 | `erlc +deterministic -o D …/sys_core_bsm.erl` | same as #2 on `sys_core_bsm.erl` | `{ok,sys_core_bsm}`; byte-identical | **agree** (6.9 s vs 0.21 s) |
| 4 | `selfhost/in_process_compile_of_the_compilers_flatten_stage` | A.9/A.2 | `erlc +deterministic -o D …/beam_flatten.erl` | same as #2 on `beam_flatten.erl` | `{ok,beam_flatten}`; byte-identical | **agree** (10.1 s vs 0.18 s) |
| 5 | `selfhost/emake_driver_compiles_the_compilers_own_source` | A.9 | `erl -noshell -eval 'R=make:all([{emake,[{Src,[deterministic,{outdir,D}]}]}]), io:format("~p~n",[R]),halt(0).'` | same argv, `beam_rt` | `Recompile: …` + `up_to_date` on both; artifact byte-identical | **agree** |
| 6 | `selfhost/emakefile_option_spelling_is_a_usage_artifact` | A.9 | `make:all([{emakefile,Src}])` | same argv, `beam_rt` | both answer `up_to_date`, no artifact on either side | **agree** (a usage trap, not a divergence) |
| 7 | `selfhost/committed_bootstrap_compiler_compiles_a_module` | A.9/A.7 | **impossible** — the oracle answers `{'EXIT',{undef,…}}`; OTP-29 cannot load the OTP-30 `compile.beam` (`{error,badfile}`, *Use of opcode 193*) | `beam_rt -pa bootstrap/lib/{kernel,stdlib,compiler}/ebin -noshell -eval 'io:format("~p~n",[code:which(compile)]), R=compile:file(Src,[deterministic,{outdir,D}]), io:format("~p~n",[R]),halt(0).'` | `code:which(compile)` ends in `bootstrap/lib/compiler/ebin/compile.beam`; `R` is exactly the string `'unsupported_opcode: put_record: create: no record definition table for another module'`; **no** artifact is written (assert the artifact's *absence*, never compare bytes of nothing) | **unsupported** (recorded gap) |
| 8 | `selfhost/erlc_frontend_reaches_the_runtime_with_the_compile_argv` | A.6/A.9 | `erlc +deterministic -o D Src` | `ERLC_EMULATOR=<beam_rt> erlc +deterministic -o D Src` | exit 1, stderr contains `beam_rt: unknown flag -S1`, no artifact; the wrapper-captured argv is the 20-token front-end argv above | **unsupported** (recorded gap) |
| 9 | `selfhost/make_driven_compile_command_line` | A.9 | `make -C lib/<app>` (the `otp.mk.in` rule) | n/a | the rule's command line, driven through `ERLC_EMULATOR`, reaches #8's refusal | **skipped** (environment only) |

### The typed error and owner of each blocked case

* **#7** — reason: the exact string
  `'unsupported_opcode: put_record: create: no record definition table for another module'`
  (returned by `compile:file/2`, i.e. *not* raised); nothing produced. Owner crate: **`beam_engine`**
  (`crates/beam_engine/src/families/records/mod.rs::local_definition`, foreign `{Module,Name}`
  create-form `put_record`); the module-record table + `code:ensure_loaded/1` trap seam belongs to
  **`beam_bifs`**/**`beam_rt`**. Corroboration: 0 `put_record` instructions in the 59 host OTP-29
  `compiler/ebin` beams (see §1).
* **#8** — reason: `beam_rt: unknown flag -S1` on stderr, exit 1, nothing produced (the runtime's
  documented argv refusal, so an `unsupported`, never a `mismatch`). Owner crate: **`beam_rt`**
  (`src/cli.rs:488` — `+S` wants a separate value token, so the compact `+S1`/`+sbtu` the front end
  sends are rejected), with the second, independent root cause
  `init:get_plain_arguments() =:= []` in **`beam_rt`** (`src/native.rs:40`, `src/native/init.rs:175`).
* **#9** — not a runtime gap: the *environment* cannot run `./configure` (`$CONFIG_SHELL` is a
  missing `/nix/store/…-bash-5.3p9/bin/bash`), so `make/$(TARGET)/otp.mk` does not exist. Recorded as
  `skipped`; it becomes meaningful the day a configured tree is available.

### Deliberately **not** a case

* **`-extra` plain arguments.** `erl -noshell -eval 'io:format("~p~n",[init:get_plain_arguments()])' -extra a b`
  prints `["a","b"]`; `beam_rt` prints `[]`. That is a **differing observable**, i.e. a `mismatch` —
  not a gap the runtime signals — so it must **not** be proposed as a corpus case while it stands:
  `erts_rust/tools/differential/divergences.md` already records it (the `-extra foo` entry, with the
  oracle's *other* shape — a SIGTERM report and a hang when `-extra` is not last), and
  `src/corpus/flags.rs`'s module doc says such forms are deliberately not pinned. The probe reports
  the value on the L5 evidence line so the fact is reproduced without turning the run red.
* **A `cmp` of anything that was not produced.** Every blocked case above asserts the *absence* of an
  artifact instead (a "byte-identical" claim on nothing is never allowed).
* **Absolute-vs-fixed module for #7.** `cerl_clauses.erl` answers `error` plus the compiler's own
  `Function: is_catchall/1` diagnostic (same gap, different surface), so it cannot carry a typed-reason
  golden.

---

## 3. How to land this in `erts_rust/tools/differential`

1. **Port the agreeing cases into `src/corpus/selfhost.rs`** (append to its `group()`; keep the
   `Group`/`Case`/`Golden` shape and the `%CopyrightBegin%` header). Cases #2–#6 are
   `CaseKind::Eval` with `Probe::Value`-style probes: the existing `selfhost/*` cases already pin
   `compile:file` and `make:all` this way, and #1 is a plain expression case. Notes that matter:
   * `{deterministic,true}` on **both** sides, or the `CInf`/`Line` chunks embed the path/cwd and the
     artifact comparison is meaningless;
   * the corpus runs with **CWD = the repo root** — every source is referenced by its relative
     `lib/compiler/src/*.erl` path, exactly as the seven landed `selfhost/` expression cases do;
   * assert the artifact bytes *relationally* (compile on the oracle, compile under `beam_rt`, compare
     the two binaries) — never pin a compiler-release artifact or an md5 value;
   * #5/#6 must use `make:all([{emake,[{Src,Opts}]}])` (`lib/tools/src/make.erl:231-237`); the
     `{emakefile,File}` spelling is silently treated as an Emakefile entry for a file named
     `emakefile`, so both sides answer `up_to_date` — that is the trap #6 pins, not a divergence.
2. **Record the two gaps in `known-gaps.txt`** (one entry per line,
   `<case-id> <owning-crate> <reason…>`), so `suite --strict --allowlist known-gaps.txt` stays green:
   ```
   selfhost/committed_bootstrap_compiler_compiles_a_module            beam_engine  a create-form put_record whose Id is a foreign {Module,Name} record reference needs the C's record definition table (erts_record_find_entry) / a code:ensure_loaded/1 trap; the tree's own OTP-30 compiler cannot compile any module until this closes
   selfhost/erlc_frontend_reaches_the_runtime_with_the_compile_argv    beam_rt      the OTP erlc front end sends the compact +S1/+sbtu forms (rejected: "+S" wants a separate value token) and relies on init:get_plain_arguments/0, which this runtime models as []
   ```
   Both entries are *closable* gaps, so they belong in the allowlist only while they reproduce; the
   ratchet must be tightened (entry deleted) in the same change that closes them. Do **not** record the
   `-extra` plain-arguments divergence here — that one is a `mismatch` and belongs in `divergences.md`
   (it already is).
3. **Drive the gate**: `erts_rust/tools/differential/gate.sh` (it derives the workspace and
   `known-gaps.txt` from its own location, so nothing worktree-specific is baked in). Exit 0 = every
   case agreed or is an allowlisted gap; exit 3 = a gap is not allowlisted (or an entry went stale);
   exit 1 = a `mismatch`. For the fast triage route use `differential list` + per-id
   `differential run … --out-file` + `differential merge --from <dir> --allowlist known-gaps.txt`,
   with CWD at the repo root and a private `TMPDIR` per case.
4. **Re-run `lib/compiler/scripts/a9_selfhost_probe.sh ladder` after any runtime change** — it is the
   cheap (≈1 min) local re-check of the two gap entries: L4/L5 going `PASS` is the signal to delete the
   allowlist entries, add the artifact comparison to case #7/#8, and tighten the ratchet.

## 4. See also

* `a9_selfhost_probe.sh` (this dir) — the harness; `CONTEXT.md` (this dir) — its measured facts.
* `../../../erts_rust/tools/differential/{known-gaps.txt,divergences.md,GAPS.md,CASE_DRAFTS.md}` —
  the allowlist, the unpinnable divergences, the generated gap report and the draft recipes.
* `../../../erts_rust/tools/differential/src/corpus/{selfhost.rs,selfhost_chain.rs}` — the 33 landed
  A.9 cases this family extends (in-process compiles, artifact-level assertions, the staged chain).
* `../../CONTEXT.md` §"Stage 2"/A.9 and `../../../erts_rust/CONTEXT.md` — the rewrite's routing.
