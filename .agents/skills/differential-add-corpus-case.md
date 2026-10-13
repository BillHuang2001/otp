---
name: differential-add-corpus-case
description: How to add a case to the differential harness's conformance corpus (tools/differential) — the Rust-registry mechanics, the id/prefix convention, the five CaseKind shapes, and the no-harness-change recipes for adding an A.3 (NIF C ABI), A.4 (driver C ABI) or A.5 (live-distribution Command) case. Load this before editing src/corpus/* or adding a case.
---
# Adding a corpus case to `tools/differential`

The corpus is **code, not a manifest**. Load this when adding/altering a case or a group.

## Registry mechanics
- Each contract group is a module `src/corpus/<group>.rs` exposing `pub(crate) fn group() -> Vec<Case>`; `src/corpus/mod.rs::build_corpus()` concatenates them into a `OnceLock<Vec<Case>>`. A new group MUST be `mod <group>;`-declared and appended in `build_corpus()`.
- The `corpus` registry tests assert well-formedness (unique ids, ≥1 contract each, per-kind goldens, the module-run argv shape + contract allow-list, only an env case sets a non-empty env, `expand_env` substitutes `{dir}` in values only) — a malformed case fails `cargo test`.

## A case id is `<group>/<name>`, but `<group>` is a *subdivision*, NOT the file name
One file may emit several prefixes: `ets_spec.rs` ⇒ `ets/…` + `ets_spec/…`; `bifs.rs` ⇒ `bifs/…` + `lists/…`; `terms.rs` ⇒ `terms/…` + `errors/…`; `printing.rs` ⇒ `print/…` + `float/…`; `phash.rs` ⇒ `phash/…` + `phash2/…`. So per-file counts are NOT derivable from id prefixes — count a group's cases by its quoted `"<prefix>/<name>"` literals. Only `wire_ids.rs` (`node_identity_cases()`) and `ets.rs` build cases in a loop.
The two ABI contract ids are `Contract::NifAbi` (A.3) and `Contract::DriverAbi` (A.4).
A **widened** group is an ordinary group that mirrors an existing one with its own id prefix (`nif_abi_extra.rs` ⇒ `nif_extra/…`, `driver_abi_extra.rs` ⇒ `driver_extra/…`, `mexec_extra.rs` ⇒ `mexec_extra/…`) and its own contract claim.

## The `Case` shape and the five kinds
`Case { id, description, contracts: &[Contract], kind: CaseKind, golden: Golden }`.
- `CaseKind::Eval { expr, extra_argv, probe }` — both runtimes get **identical** argv.
- `CaseKind::EvalEnv { expr, extra_argv, env, probe }` — `Eval` + an explicit `'static` env (the `ERL_*FLAGS` surface).
- `CaseKind::Module(ModuleRun { source, module, function, arity, args: &[i64], nif })` — the embedded module is compiled with `erlc`; the oracle runs `-pa`+`-eval`, the Rust runtime `beam_rt run`, with **integer args only** (stdout + exit compared, stderr not).
- `CaseKind::ModuleEnv { run, extra, env }` — `Module` + the case env (its `{dir}` placeholder expanded to the compiled dir) and `extra` helper modules compiled into the same dir; the oracle argv carries **no** `-pa`, so the dir reaches the code path only via the env.
- `CaseKind::Command(CommandRun { oracle, rust })` — the non-CLI stage: each side is a `CommandSpec { program: CommandProgram::{Oracle,Rust,Helper}, args }` with its own argv; only stdout + exit status are compared.

`Golden { stdout: Option<&'static str>, exit_code }` is the **oracle's** expected stdout *line* (no trailing newline) — the corpus's own drift self-test. Keep every golden **order-stable**: never assert a multi-key-map `~w`, a per-VM-start pid/port/ref/creation value, or a temp-dir-bearing `load_nif` message — assert a shape / `maps:size` / sorted keys, or classify into an integer.

## Adding an A.3 (NIF C ABI) case — no harness change
- Reuse the `nif_so` shape: a `CaseKind::Module` case carrying `Some(NifFixture::CompiledLibrary { name, c_source })`. `module_build::build` compiles the C with `cc -shared -fPIC -I<erts include>` into the case's own temp dir; the module self-locates `<dir>/<name>` (suffix-less, so both runtimes append `.so`).
- The `nif_abi`/`nif_abi_extra` groups widen this: one shared C library whose only `ErlNifFunc` entry is `probe/1`, driven by an integer selector through an **external** call — one new `enif_*` behaviour is one C `switch` case plus one corpus entry, every golden a VM-stable integer.
- **Hard constraint: the C may only reference `enif_*` symbols the Rust runtime really defines.** Both runtimes `dlopen(RTLD_NOW)` the same `.so` and the host `erl_nif.h` declares each `enif_*` as a direct `extern` symbol, so a symbol the Rust runtime does not export makes the *Rust* load fail (`{error,{load_failed,…}}`) while the oracle loads it — a **mismatch**, not a gap. The exported set is the `pub extern "C" fn enif_*` in `../../crates/beam_abi/src/nif/enif/` (the slots `enif_api_table()` fills). A resource **destructor** stays unpinnable (the oracle schedules it on a misc aux-work thread).
- Load rules a NIF case must honor (pinned by `nif_so`): call `erlang:load_nif/2` from the NIF module itself; stub **and export** every function in the library's `ErlNifFunc`; make the NIF calls **external**; classify the `{error,{Reason,_}}` tag to an integer (never print the path-bearing `Text`).

## Adding an A.4 (driver C ABI) case — no harness change
- The `driver_c` group loads a **real C driver `.so`**: a module-run case claims `Contract::DriverAbi` and carries `Some(NifFixture::CompiledLibrary { .. })`, compiled against the erts include dir (which also holds `erl_driver.h`). The module opens the driver as `open_port({spawn_driver, Dir ++ "/<name>"}, …)`: the Rust runtime's `driver_lookup` treats a name containing `/` as the object path and validates the entry name against the file stem; the oracle needs `erl_ddll:load_driver/2`, so `driver_c`'s `open/2` tries the path form and falls back to the loaded bare name. Fixture pattern: `../../crates/beam_io/tests/fixtures/host_driver.c`. (The `driver/*` group separately drives the **built-in** `spawn`/`spawn_executable` drivers.)
- The `driver_abi`/`driver_abi_extra` groups widen this: one shared C driver `.so` whose `output` callback dispatches on a one-byte `port_command/2` selector — a status byte, a `driver_output*` byte delivery (answered as the base-256 integer), or a `driver_select`/`driver_set_timer` arm answered later by `ready_input`/`ready_output`/`timeout`. One new slot = one selector case.
- The driver echo round trip needs `beam_rt run` to drive the full scheduler (`Runtime::run_scheduled`); a single engine pass blocks the receiver and the CLI exits `3` (`beam_rt: unsupported: ... blocked`) — a gap, never a mismatch, never a hang.

## Adding an A.5 (live distribution) case — the `Command` kind
- The `dist/` group (`src/corpus/dist.rs`, 3 cases) pins a live two-node loopback as `CaseKind::Command`: the oracle side spawns the system `erl` with a `peer:start_link/1` script; the Rust side drives the external **`dist_probe`** helper (`crates/beam_dist/src/bin/dist_probe.rs`, modes `loopback ping|nodes|term`), whose stdout is a host-independent observable (`pong` / the peer count / the canonical ETF hex image). The helper reports a gap the usual way (`dist_probe: unsupported: <reason>` on stderr, exit 3) and a **missing** helper is a loud `Skipped` — never a fabricated pass. `gate.sh` builds `-p beam_dist` so the helper exists.
- Each oracle script starts its own `distoraN` node + `distpeerN` peer so the cases never collide on `epmd`; they run in one lane (they hold the corpus run lock with the other distribution cases).
