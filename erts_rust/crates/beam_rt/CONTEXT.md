# `beam_rt` — Rust ERTS Runtime Binary (boot · CLI · `system_info` · runtime glue)

## Intent
The **top of the workspace**: this crate produces the `beam_rt` executable and is the only crate that
sees the whole picture. It owns the parts of ERTS that no subsystem crate can own alone:

* the **executable entry point** (`main.rs`) and the **CLI flag surface** — compatibility contract
  **A.6** (`-version`, `-noshell`, `-eval`, `-s`, `-pa`/`-pz`, `+S`/`+A`/`+P`, …);
* the **boot sequence** and the **preloaded-module registry skeleton** — contract **A.7**;
* **`system_info/1`** and the runtime introspection surface — contract **A.8**;
* and — the key deliverable — the **concrete `ExecutionContext` glue** that adapts the engine's
  seam types onto the real `beam_bifs` registry / `beam_alloc` heap / `beam_process` pid, turning the
  Stage-1 interpreter into a runnable runtime.

**This crate is the vertical slice.** The engine-level slice already works
(`../beam_engine/tests/slice.rs` runs the committed `slice.beam` on a test-only `MinimalContext`);
`beam_rt` makes the *same* thing happen through the real runtime glue and through the executable
end to end. `Runtime::run_function` / `Runtime::run_module` are the evidence.

It depends on **all** other crates (wave 6, `../CONTEXT.md`) but must **call down into them, never
duplicate their logic**: BIFs come from `beam_bifs`, terms/heaps from `beam_term`/`beam_alloc`,
pids from `beam_process`, modules from `beam_loader`, execution from `beam_engine`.

Legacy C ground truth (read-only, never modify): `../../erts/emulator/sys/unix/sys.c` (`main`),
`../../erts/etc/common/erlexec.c` (the `erl` flag wrapper), `../../erts/emulator/beam/erl_init.c`
(boot), `../../erts/emulator/beam/erl_bif_info.c` (`system_info` keys),
`../../erts/preloaded/src/erlang.erl`. Cite what is relied on.

## API Surface (FROZEN — the runtime façade dependents and tests design against)
Re-exported at the crate root and via `beam_rt::prelude::*`.

### `runtime.rs` — the façade
```rust
pub struct Runtime { /* config, registry, atoms, processes, preloaded, heap, info_heap, tables, self_pid, modules, booted */ }

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Result<Runtime>;
    pub fn with_defaults() -> Result<Runtime>;                 // RuntimeConfig::default(); booted on demand
    pub fn boot(&mut self) -> Result<()>;                       // the A.7 boot sequence (idempotent)
    pub fn is_booted(&self) -> bool;
    pub fn config(&self) -> &RuntimeConfig;

    /// Load a `.beam` and cache it (owns its module-local `AtomTable`).
    pub fn load_module(&mut self, path: impl AsRef<Path>) -> Result<&Module>;
    /// A cached module by name.
    pub fn module(&self, name: &str) -> Option<&Module>;
    /// The preloaded-module registry skeleton (A.7).
    pub fn preloaded(&self) -> &PreloadedModules;

    /// THE SLICE (stdout): load `path` and run `function/arity`; returns `x(0)`.
    pub fn run_function(&mut self, path: impl AsRef<Path>, function: &str, arity: u32,
                        args: &[Eterm]) -> Result<Eterm>;
    /// The general primitive: run against a caller-owned module with an explicit sink.
    pub fn run_module(&mut self, module: &Module, function: &str, arity: u32, args: &[Eterm],
                      out: &mut dyn OutputSink) -> Result<Eterm>;

    /// `system_info/1` (A.8) materialized as a term on the runtime scratch heap.
    pub fn system_info(&mut self, key: &str) -> Result<Eterm>;
    /// `system_info/1` as a heap-independent value (the testable source of truth).
    pub fn system_info_value(&self, key: &str) -> Result<InfoValue>;

    /// Evaluate an Erlang expression (`erl_eval`-style tree-walk; A.6).
    pub fn eval(&mut self, expr: &str) -> Result<Eterm>;

    /// Print a term via the registered `io:format` `~p` printer (no second printer).
    pub fn format_term(&mut self, term: Eterm, module: Option<&Module>) -> Result<String>;
}
```

### `context.rs` — the concrete `ExecutionContext` (the glue)
```rust
pub struct RuntimeContext<'a> { /* runtime: &'a mut Runtime, output: Option<&'a mut dyn OutputSink> */ }

impl<'a> RuntimeContext<'a> {
    pub fn new(runtime: &'a mut Runtime) -> Self;
    pub fn with_output(runtime: &'a mut Runtime, sink: &'a mut dyn OutputSink) -> Self;
    pub fn heap_available(&self) -> usize;
    pub(crate) fn run_export(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm>;
}
impl ExecutionContext for RuntimeContext<'_> {
    // call_bif  → RuntimeContext::try_native (halt/display/io:put_chars/apply/system_info/
    //             init:get_argument,init:get_arguments,init:get_plain_arguments/erlang:display/
    //             os:cmd/code:get_path,code:all_loaded,code:which,code:lib_dir)
    //             → `BifRegistry::contains` → `BifRegistry::call` (a `BifContext` over
    //             {heap, code_server atoms, self_pid, tables, output, PendingInterner}); else if
    //             module == "erlang" → `error::unknown_bif`; else seed the system code path
    //             (`Runtime::find_module_file`) then `code_server.load(module)` → `run_export`.
    // apply_fun → an `-eval` closure (`terms::fun_uniq == eval::EVAL_FUN_UNIQ`) goes to
    //             `eval::apply_eval_closure`; otherwise resolve the fun's module
    //             (`terms::is_fun/fun_uniq/fun_index/fun_free` + a matching
    //             `FunDescriptor { uniq, index }`) across `code_server.modules()`, then
    //             `Machine::new().run_label(...)`.
    // ensure_heap → Ok when `heap_available() >= heap_need`, else `BeamError::SystemLimit`
    //             (M1 has no GC — see Known Issues).
}
```
`call_bif` builds the `BifContext` with a **`PendingInterner`**: a BIF that must create a *new* atom
(`binary_to_term`/`list_to_atom`/`binary_to_atom`) records the name; the runtime commits the recorded
names into the **one shared table** in order after the BIF returns (their predicted indices are exact
— interning is append-only and single-threaded). This is what makes a `binary_to_term`-decoded atom
nameable by the printer/comparison — see Design Decisions.
This is the **only** place the engine's seam meets the Stage-5 core; it keeps the Stage-1/Stage-5
DAG acyclic (`../CONTEXT.md`).

### `config.rs` — parsed configuration
```rust
pub type EmulatorConfig = beam_common::config::RuntimeConfig;   // the A.6 skeleton (flavor, +S, +P, …)
pub struct InitArgs { /* ordered flag → occurrences; record/get/len/is_empty/occurrences */ }
pub enum ArgTerm { Int(i64), Atom(String), Str(String), List(Vec<ArgTerm>) }
pub enum Action { Boot, Version, Eval(String), Apply { module: String, function: String, args: Vec<String> },
                  Run { module: String, function: String, args: Vec<String> },
                  RunFile { file: PathBuf, module: String, function: String, arity: u32, args: Vec<ArgTerm> } }
pub struct RuntimeConfig { pub emulator: EmulatorConfig, pub code_paths: Vec<PathBuf>,
                           pub heap_words: usize, pub action: Action, pub init_args: InitArgs }  // Default + `with_*` setters
```

### `cli.rs` — the CLI parser + driver (contract A.6)
```rust
pub const USAGE: &str;
pub const EXIT_OK: i32 = 0;          // success
pub const EXIT_RUNTIME: i32 = 1;     // genuine runtime error
pub const EXIT_USAGE: i32 = 2;       // malformed invocation / usage error
pub const EXIT_UNSUPPORTED: i32 = 3; // unsupported: a Milestone-1 capability gap
pub fn parse(args: &[String]) -> Result<RuntimeConfig>;   // `args` excludes argv[0]
pub fn run(args: Vec<String>) -> i32;                     // parse → Runtime → dispatch → exit code
```

### `boot.rs`, `info.rs`, `format.rs`
```rust
// boot.rs (A.7)
pub struct PreloadedModules { /* name set */ }
impl PreloadedModules { pub fn standard() -> PreloadedModules; pub fn is_preloaded(&self, name: &str) -> bool;
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_; pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool; }

// info.rs (A.8)
pub const SUPPORTED_KEYS: &[&str];                              // the 12 answered keys
pub const INFO_ATOMS: &[&str];                                  // the atom vocabulary the surface needs seeded
pub enum InfoValue { Int(i64), Str(String), Atom(&'static str), Bool(bool), Tuple(&'static str, &'static str) }
impl InfoValue { pub fn to_eterm(&self, heap: &mut Heap, atoms: &AtomTable) -> Result<Eterm>; }
pub fn system_info_value(key: &str, rt: &Runtime) -> Result<InfoValue>;

// format.rs
pub fn term_to_string(registry: &BifRegistry, heap: &mut Heap, atoms: &AtomTable,
                      self_pid: ProcessId, term: Eterm) -> Result<String>;
```

## CLI subset (contract A.6 — the *exact* supported surface; anything else is *unsupported*, exit 3)
| Switch | Meaning in `beam_rt` |
|---|---|
| `-version`, `--version` | print the version banner (OTP `30.0-rc0` / erts `17.1`) and exit 0 |
| `-noshell` | run head-less (M1 default; accepted, no interactive shell exists) |
| `-eval Expr` | evaluate a **general Erlang expression** (`erl_eval`-style) and print **nothing** — the result is discarded, exactly like real `erl` (`erl -eval '2+3'` prints nothing, exits 0); one trailing `.` is tolerated and stripped |
| `-s Mod Func [Args...]` | apply `Mod:Func(a, b, …)` (Arguments are **atoms**); the result is **discarded** (real `erl` semantics); exit 0 on success |
| `-run Mod Func [Args...]` | same, but Arguments are plain **strings**, materialized as Erlang char lists; the result is **discarded** |
| `-pa Dir` | add `Dir` to the code path (searched **before** `-pz` dirs) |
| `-pz Dir` | add `Dir` to the code path (searched after) |
| `+S N[:N]` | normal scheduler count → `emulator.schedulers` (first number); M1 runs single-threaded |
| `+A N` | async-thread pool size → preserved verbatim in `emulator.extra_emulator_flags` |
| `+P N` | max process count → `emulator.max_processes` |
| `-name N` / `-sname N` | node name → `emulator.node_name` (distribution M1 = skeleton) |
| `run <file.beam> <mod>:<func>/<arity> [int-arg…]` | **the vertical slice**: load the file, run, **print** the result |

* `-eval` grammar: the Erlang **expression** subset (`src/eval/{parse,ast,mod}.rs`) — literals in every
  base, lists/tuples/maps/binaries, the full operator set with Erlang precedence,
  `case`/`if`/`begin`/`try`/`catch`/`receive`, `fun`s and comprehensions — evaluated by an
  `erl_eval`-style tree-walk that dispatches arithmetic/comparison to the **`beam_bifs` `erlang`
  registry** (semantics are never re-implemented). An expression outside the grammar is a **capability
  gap** (see exit code 3 below), not a genuine error.
* **Every command-line flag is an init argument.** Each recognized switch *and* each unrecognized
  `-flag`/`+flag` (name = the flag with the leading `-`/`+` run removed; values = the following tokens
  that do not start with `-`/`+`) is recorded as an [`InitArgs`] occurrence — the source of
  `init:get_argument/1`, e.g. `-noshell` ⇒ `{ok,[[]]}`, `-pa /a -pa /b` ⇒ `{ok,[["/a"],["/b"]]}`.
  An unmodeled switch is therefore **data, not a usage error**: real `erl -noshell --nope -eval
  'halt(0).'` exits 0. Only a **stray positional argument** (outside `run`) is a usage error (exit 2).
* `run` argument grammar (each `ARG` token): a **signed integer** (`42`, `-3`) → `ArgTerm::Int`; a
  **double-quoted string** (`"a\n"`, escapes `\n \t \r \\ \"`) → `ArgTerm::Str` (an Erlang char list);
  a **list** (`[1, atom, "x", [2]]`, top-level commas) → `ArgTerm::List`; otherwise a bare atom name →
  `ArgTerm::Atom`. The materialized count must equal the arity (else exit 2). A `-`/`+`-prefixed token
  that is not an integer is a usage error.
* `run`, `-s` and `-run` resolve modules through `code_paths` (`-pa`/`-pz`) + the system stdlib and the
  file argument; M1 searches for `<Mod>.beam` in the code path.

## Exit codes & the `unsupported` signal (the stable external contract)
The CLI exposes **four** exit codes; the differential harness keys off this mapping.

| Code | Meaning | stderr |
|---|---|---|
| `0` (`EXIT_OK`) | success | — |
| `1` (`EXIT_RUNTIME`) | genuine runtime error (a load/execute/BIF failure, e.g. a missing `.beam`, module-not-found, unknown function, BIF `system_limit`) | `beam_rt: <error>` |
| `2` (`EXIT_USAGE`) | malformed invocation / usage error (a missing operand of a *known* switch, a malformed `run` spec, a stray positional argument, a malformed numeric switch value) | `beam_rt: <error>` then [`USAGE`] |
| `3` (`EXIT_UNSUPPORTED`) | **unsupported** — a Milestone-1 capability gap | exactly one line: `beam_rt: unsupported: <reason>` (no `USAGE`) |

Unsupported is emitted for:
1. a `-eval` expression outside the supported grammar (`src/eval/parse.rs`);
2. a BEAM-engine capability gap — a raise whose reason atom starts with `unsupported_opcode`;
3. a `beam_bifs` capability gap — the reason atom `unsupported_bif`.

A capability gap is **uncatchable**: in `src/eval/mod.rs` `flow_from_error` turns it into `Flow::Fatal`
(like `halt`), so the self-terminating `try (EXPR) of … catch C:R -> … end` probe the differential
harness wraps its expressions in **cannot** swallow it and print it as an apparent `exception error …`.
It escapes the evaluator, reaches `cli::run`, and becomes the exit-3 `beam_rt: unsupported: <reason>`
signal — so an unimplemented feature is honestly `unsupported`, never a false `error`, in the gate.

**Classification convention.** A capability gap is signalled by a raise whose **reason atom starts
with `unsupported`**. `src/error.rs::unsupported_reason` is the **single** place that convention is
interpreted: the constructors there (`unsupported`, `bad_eval`) and the sibling crates (`beam_engine`
`unsupported_opcode: <what>`, `beam_bifs` `unsupported_bif`) all follow it. It strips the leading `unsupported` token and any following `:`/`_`/space (`unsupported: X`→`X`,
`unsupported_opcode: Y`→`opcode: Y`, `unsupported_bif`→`bif`) and returns `Some(reason)`; the CLI
turns that into the `beam_rt: unsupported: <reason>` line and exit 3. Classification must go through
`unsupported_reason` (which reads `ExitReason::as_atom`) rather than `BeamError::as_atom`, because the
latter returns `None` for atoms backed by an **owned** `String` — which every message here is.

## `system_info/1` keys (contract A.8)
`InfoValue` is the heap-free source of truth; `Runtime::system_info` materializes it on the scratch
heap. Strings are Erlang **strings** (lists of char small-ints) exactly as the oracle returns them
(`erlang:system_info(version)` → `"17.1"`, verified against OTP 29/erts-17.1).

| Key | Value |
|---|---|
| `version` | `Str(beam_common::version::ERTS_VERSION)` → `"17.1"` |
| `otp_release` | `Str` major of `OTP_VERSION` → `"30"` |
| `system_architecture` | `Str` target triple, best-effort from `std::env::consts` (`"x86_64-pc-linux-gnu"`) |
| `system_version` | `Str"Erlang/OTP {major} [erts-{erts}] [source] [{bits}-bit]"` |
| `nif_version` | `Str"2.18"` from `beam_abi::{ERL_NIF_MAJOR_VERSION,ERL_NIF_MINOR_VERSION}` |
| `wordsize` | `Int(size_of::<usize>())` → `8` |
| `logical_processors` | `Int(available_parallelism)` |
| `process_count` | `Int(rt.processes.len())` (the M1 process table) |
| `os_type` | `Tuple("unix","linux")` (or `("win32","nt")`) |
| `threads` | `Bool(true)` → atom `true`/`false` |
| `emu_flavor` | `Atom(rt.config().emulator.flavor.as_str())` → `jit` by default (the baseline JIT), `emu` for a `Flavor::Emu` config |
| `schedulers` | `Int` — an explicit `+S N` wins; `+S 0`/unset falls through to `available_parallelism()`; always `> 0` |
| anything else | `Err(BeamError::BadArg)` — matches the oracle's `error:badarg` |

`src/info.rs` also exports `INFO_ATOMS` — the complete atom vocabulary of the surface
(`unix,linux,win32,nt,true,false,jit,emu`). `seed_core_atoms` (`src/boot.rs`) interns it, because
`InfoValue::to_eterm` resolves atoms **through the runtime's atom table** and reports
`BeamError::TermNotFound` rather than inventing a term — so a key whose value is an atom only agrees
once that atom is seeded. Add any new atom-valued key's atom to `INFO_ATOMS`.

## Constraints
* **Errors are return-based, workspace-wide** (`../CONTEXT.md` §3): every fallible API returns
  `beam_common::Result`. **No crate-local error enum**, **no `panic!`/`unwrap()` on runtime data**,
  **no silent swallowing**. CLI diagnostics are carried as `BeamError::Raise(Raise::error(ExitReason::atom(msg)))`
  (the workspace's message-carrying error; `BeamError: Display` renders it) — see `error.rs`.
* **`unsafe`-free.** The whole crate is `#![deny(unsafe_code)]`: the glue is built only on the safe
  peripheries of the lower crates (`Heap::cons/tuple`, `AtomTable::intern`, `BifRegistry::call`).
* **Call down, never duplicate.** BIFs → `beam_bifs`; term build/inspect → `beam_term`; heap →
  `beam_alloc`; `.beam` → `beam_loader`; execution → `beam_engine`; pids/process table →
  `beam_process`. The `~p` printer is **not** re-implemented — `format.rs` drives the registered
  `io:format/2` BIF.
* **Frozen term layout** (`../CONTEXT.md` §2): terms are built/inspected only through `beam_term`.
* **Files ~1000 lines**, split by responsibility. `#![warn(missing_docs)]`; every public item documented.

## Design Decisions
* **One shared atom table (the code server's).** There is **no** separate module-local / global split:
  `CodeServer` owns **one** `AtomTable`, every module it loads is interned into it
  (`CodeServer::load_bytes` re-runs the loader pipeline seeded with that table), and
  `RuntimeContext::call_bif` hands `beam_bifs`' `BifContext` that same table. So an atom a BIF builds
  is an index the engine compares against (`beam_engine`'s `cmp(.., &module.atom_table)`, whose prefix
  is the shared table) and the printer (`src/format.rs`) names, for **every** loaded module —
  `Runtime::intern`/`atoms` are thin delegates to it. Indices are assigned in insertion order and never
  renumber. A BIF that must *create* a new atom (`binary_to_term`, `list_to_atom`, `binary_to_atom`)
  goes through `PendingInterner`: `BifContext` borrows the table immutably, so the interner records the
  name (predicting `base + pending` — exact because interning is append-only and single-threaded) and
  the runtime commits the recorded names into the shared table, in order, right after the call — at
  exactly the indices the BIF already used. This is what makes a `binary_to_term`-decoded atom
  nameable (`wire/decode_reference_atom_blob` / `..._map_blob` agree with the oracle).
* **Heap lifetime.** `Runtime` **owns** the process heap (`heap`) so a returned `Eterm` stays valid
  after `run_*` returns (the `RuntimeContext` only borrows it). `system_info`/`format_term` use a
  separate `info_heap`, `reset()` at the start of each call — so a term returned by `system_info` is
  valid **until the next `system_info`/`format_term` call**. Both documented on the methods.
* **`ensure_heap` without GC.** M1 has no collector in the engine (`../beam_engine` Known Issues), so
  `ensure_heap` checks capacity and returns `BeamError::SystemLimit` when short; the default
  `heap_words` (64 Ki words) is ample for the slice. Growth-by-collection is Stage 4 (`beam_gc`).
* **CLI is a documented subset, never a silent no-op.** Unmodeled emulator flags are **preserved
  verbatim** in `emulator.extra_emulator_flags` (the `beam_common` A.6 skeleton contract); a switch
  that is *not* part of this crate's documented surface is a **capability gap** reported as
  *unsupported* (exit 3) naming it, not a usage error. `-eval` and `run` reuse the BIF registry rather
  than re-implementing arithmetic/printing.
* **`system_info` returns `Eterm` via `InfoValue`.** The value is computed heap-independently
  (`InfoValue`) and materialized once (`InfoValue::to_eterm`); this makes the keys unit-testable
  without a heap and keeps the materialization in one place.
* **`main.rs` is a thin shim** over `cli::run(argv)`; all logic lives in the library so it is unit- and
  integration-testable.
* **The `FunCaller` seam (`FunBridge`).** `beam_bifs` applies a user fun through `BifContext::call_fun`
  (default: `unsupported_bif`, i.e. every `fun`-taking `lists:*`/`maps:*` BIF raises until a caller is
  installed); `beam_rt` installs a `FunBridge` there. A `FunCaller` needs `&mut Runtime` while
  `BifContext` still borrows runtime state — two `&mut` on one value is impossible — so **every**
  `BifContext` resource is made a non-runtime-borrowing clone: `Rc<BifRegistry>`, an `Rc<AtomTable>`
  atom handle (`CodeServer::atoms_handle`, copy-on-write on intern), an `Rc<RefCell<TableRegistry>>`,
  a **pooled** `beam_alloc::Heap` (`take_bif_heap`/`put_bif_heap`, stack discipline — a nested call
  takes a fresh heap) and the disjoint `output` field. That leaves `FunBridge` as the only
  `&mut Runtime` borrow in `call_registered`. `FunBridge::call_fun` delegates to the **existing**
  `RuntimeContext::apply_fun` (an `-eval` closure via `eval::apply_eval_closure`, a `.beam` fun via the
  engine's `Machine::run_label`) — the evaluator is never re-implemented. This is what makes
  `lists:map(fun(X) -> X*X end, [1,2,3])` → `[1,4,9]`, including a loaded module applying an `-eval`
  fun (`eval::tests::a_loaded_module_applies_an_eval_closure`).
* **Atom-table sharing is by handle (`Rc<AtomTable>`), interned copy-on-write.** `CodeServer` owns the
  table in an `Rc`; `atoms_mut` is `Rc::make_mut` and `atoms_handle` a cheap `Rc` clone. A `BifContext`
  can therefore name atoms without borrowing the runtime, and a commit after a BIF call that grew the
  table clones it (COW) while preserving **every** existing index — safe because interning is
  append-only and single-threaded.
* **`nif_error` raises the live reason term.** `beam_bifs`' `erlang:nif_error/1,2` build a `Structured`
  reason that carries no `Eterm`, which would print a generic placeholder. `src/eval/mod.rs` therefore
  intercepts `nif_error/1,2` and raises `Flow::Raise { class: Error, reason: <the argument term> }`, so
  the **exact** term survives (`nif_error(foo)` → `exception error foo`;
  `nif_error({nif_not_loaded,m,f,0})` → the exact tuple) and the raise stays catchable. The same
  term-carrying `Flow::Raise` path is used for `error`/`exit`/`throw` with a live reason.

## Test Strategy
* **Unit tests** (`#[cfg(test)]` per module): CLI parsing (every supported switch + `init_args`
  recording of recognized *and* unmodeled switches, the `run` argument grammar, the `-eval`
  trailing-dot, `-run`), `system_info_value` for every key + the unknown-key `badarg`, `InfoValue`
  → `Eterm` materialization (`wordsize` int, `version` string-list), `PreloadedModules`, the
  evaluator (`2+3`→5, `2*3+4`→10, `(1+2)*3`→9, `case`/`catch`/`fun`/comprehension cases, malformed
  input → typed gap), CLI `+S/+P/-pa/-pz` mapping,
  the native floor (`erlang:halt/0,1` records `Runtime::halt`, `init:get_argument` occurrences/absent),
  `error::unsupported_reason` (`Some` for `bad_eval`/`unsupported_opcode`/`unsupported_bif`, `None` for
  `missing_value`/`unexpected_argument`/`BadArg`/`SystemLimit`) and `error::halt_status`.
* **`tests/slice.rs` — the end-to-end library slice (the key evidence).** Loads the committed oracle
  fixture `../beam_loader/tests/fixtures/slice.beam` (reused by relative path via
  `concat!(env!("CARGO_MANIFEST_DIR"), …)` — a single source of truth, never copied) and asserts
  `run_function(path, "add", 2, [2,3]) == 5` and `run_function(path, "fib", 1, [10]) == 55`, plus
  `pick/1` (atom argument) — all through the **real** `RuntimeContext`/`BifRegistry`.
* **`tests/cli.rs` — the executable end-to-end.** Invokes the built binary via
  `env!("CARGO_BIN_EXE_beam_rt")` with `std::process::Command`:
  `run <slice.beam> slice:add/2 2 3` → stdout contains `5`;
  `run <slice.beam> slice:fib/1 10` → stdout contains `55`;
  `--version` → exit 0 + the version string; `-eval 2+3` → exit 0 with **empty** stdout and stderr;
  an **unknown switch** → exit 0 (recorded as an init argument); `-eval` outside the grammar → exit 3
  + exactly one `beam_rt: unsupported:` stderr line and **no** `Usage:`; a missing `.beam` → exit 1;
  a missing switch operand or a stray positional argument → exit 2 + `Usage:`.
* **`tests/eval.rs` — the `-eval` evaluator end-to-end (the differential corpus).** Reproduces all
  **17** `Eval` probes of `tools/differential`'s corpus (`terms`/`etf`/`bifs`/`system_info`/`flags`) at
  the CLI boundary, using the **same** self-terminating `try … catch` wrapper and argv the harness
  builds (`-noshell [extra…] -eval <probe>`), and asserts the **exact** stdout + exit status of each.
  This pins the corpus goldens without needing the oracle `erl` at run time, so a regression in the
  parser/evaluator/printer/exit-code path is caught even where `erl` is absent. Adds a control-flow
  extras test (`case`/comprehension/`fun`/`element`/`abs`, goldens verified against system `erl`) and
  an unsupported-classification check.
* **`tests/multimodule.rs` — the multi-module host.** Compiles a fixture with the system `erlc` into a
  scratch dir and runs it through `Runtime::run_function`: `lists:reverse/1` → `[3,2,1]`,
  `maps:get/2` on a map literal → `7`, and the on-demand stdlib module `ordsets`. It **skips loudly**
  (printing the reason) when the system toolchain/stdlib is unavailable, never silently passing.
* **Gate:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt` GREEN;
  the whole-workspace `build`/`test` must stay green.

## Known Issues / Intentional M1 gaps
* **No interactive shell, no distribution, no node.** `-noshell` is the only mode; `-name`/`-sname`
  are parsed and stored but distribution (A.5) is not started.
* **Preloaded modules are a name-set skeleton.** Boot does **not** load preloaded `.beam`s, and the
  shared atom table is seeded with `CORE_ATOMS` + `INFO_ATOMS` (not the full `atom.names` ordering), so
  positional atom indices for preloaded code are not reproduced. The erts app's `ebin` **is** on the
  code path (`CodeServer::resolve_system_ebins`), so a preloaded module (`init`, `persistent_term`, …)
  *can* be loaded from disk on demand; boot order and the registry are in place.
* **`-eval` grammar gaps left as honest *unsupported*** (exit 3, never a wrong result):
  entering a non-byte-aligned binary segment literal (e.g. `<<1:1>>`), and a binary comprehension
  (`<< Seg || <<X>> <= Bin >>` — the parser raises on the generator). Both are classified by
  `unsupported_reason`, not reported as exceptions.
* **Single process.** `system_info(process_count)` reflects the single M1 process table entry; there is
  no scheduler loop driving spawned processes yet (`beam_process` owns it).
* **`-eval`/`-s`/`-run` are documented subsets** (`-s` uses atoms + discards the result, `-run` uses
  strings; `-eval` evaluates the expression subset and prints nothing). An `-eval` expression outside
  the grammar is *unsupported* (exit 3); the M1 expression gaps (`!` send, a bare `receive`) are typed
  errors in `src/eval/mod.rs`. A capability gap (`unsupported_opcode:`/`unsupported_bif`/`bad_eval`)
  is **uncatchable**: it travels as `Flow::Fatal` (like `halt`) so a `try`/`catch` in the probe cannot
  turn it into an apparent exception, and the CLI exits 3. Real `erl`
  start-script processing, `-init_debug` and the rest are absent — and, like any unmodeled switch, are
  recorded as init arguments rather than acted on.
* **Nested-BIF re-entrancy gaps (a `fun` applied inside a BIF).** When a `fun`-taking `lists:*`/`maps:*`
  BIF calls back through `FunBridge`, the nested `RuntimeContext` takes a **fresh pooled heap** and
  passes **no** `TableRegistry` — a re-entrant borrow is impossible and a panic is forbidden, so the
  guard is `try_borrow_mut().ok()` and the tables are simply omitted: `ets:*` inside such a `fun` raises
  `unsupported_bif` (never a wrong result). The nested context also carries **no** output sink (the
  outer call holds it), so nested `io:format` writes to stdout rather than the caller's sink. Both are
  honest M1 gaps, not silent misbehavior.
* **Heap has no GC** (see Design Decisions) — a runaway allocation returns `SystemLimit`.

## Routing Table
* `src/lib.rs` → crate docs, `#![deny(unsafe_code)]`, module wiring, root re-exports, `prelude`
* `src/error.rs` → `pub(crate)` `BeamError` constructors (missing value, unexpected argument, bad spec, arity mismatch, bad run argument, unknown module/function/info key/bif, unsupported, bad eval) + the halt helpers (`HALT_PREFIX`, `halt`/`halt_status`) + `unsupported_reason` (the single capability-gap classifier) — no crate-local error enum
* `src/config.rs` → `RuntimeConfig`, `Action`, `ArgTerm`, `InitArgs`, `EmulatorConfig` alias, defaults/setters
* `src/cli.rs` → `USAGE`, `parse` (the A.6 parser + the `run` argument grammar), `run` (the CLI driver + the four exit-code constants `EXIT_OK`/`EXIT_RUNTIME`/`EXIT_USAGE`/`EXIT_UNSUPPORTED`)
* `src/codeserver.rs` → `CodeServer`: the **one** shared atom table (held as an `Rc<AtomTable>`, copy-on-write on intern — see Design Decisions) + module cache + code path + `load`/`load_path`/`load_bytes`/`add_system_paths`/`find_file`/`loaded_path`/`lib_root`/`app_dir`/`atoms_handle` (the load-on-demand host); `resolve_system_ebins` puts the stdlib, kernel **and erts** `ebin` dirs on the path (the erts app is where the preloaded modules' `.beam`s — `init`, `persistent_term`, … — live), all resolved at run time (never baked in)
* `src/boot.rs` → `PreloadedModules` skeleton (A.7) + the core-atom seed (also interns `info::INFO_ATOMS`) + the boot banner
* `src/info.rs` → `SUPPORTED_KEYS`, `INFO_ATOMS`, `InfoValue`, `system_info_value` (A.8 keys), `InfoValue::to_eterm`
* `src/context.rs` → `RuntimeContext`: the concrete `ExecutionContext` glue (native floor → `beam_bifs` registry → load-on-demand export dispatch; `apply_fun` routes an `-eval` fun (`EVAL_FUN_UNIQ`) to `eval::apply_eval_closure`; heap check); `PendingInterner` interns a BIF-built new atom into the one shared table (predicted index, committed after the call); `FunBridge` — the concrete `beam_bifs::FunCaller` the `fun`-taking `lists:*`/`maps:*` BIFs call back through
* `src/native.rs` → the runtime-native floor (`erlang:halt/0,1`, `erlang:display/1`, `io:put_chars/1,2`, `erlang:apply/2,3`, `erlang:system_info/1`, `init:get_argument/1`, `init:get_arguments/0`, `init:get_plain_arguments/0`, `os:cmd/1`, `code:get_path/0`/`all_loaded/0`/`which/1`/`lib_dir/0,1` — always consulted before the registry)
* `src/eval/mod.rs` → the `-eval` evaluator: an `erl_eval`-style tree-walk over the AST with its own `Flow` control signal (live raise reasons; `halt` **and** a capability gap are uncatchable), dispatching calls through `RuntimeContext`; `apply_eval_closure` runs a stored closure when a loaded module applies an `-eval` fun
* `src/eval/ast.rs` → the `-eval` AST (`Expr`/`Clause`/`Try`/`Seg`/`Qual`/`MapField`/`BinOp`/`UnOp`)
* `src/eval/parse.rs` → the `-eval` lexer + recursive-descent/Pratt parser (Erlang precedence; malformed input → `error::bad_eval`, exit 3)
* `src/format.rs` → `term_to_string`/`term_to_string_w` — drives the registered `io:format/2` `~p`/`~w` printer
* `src/runtime.rs` → `Runtime`: boot-wiring, code server, module cache, `run_function`/`run_module`/`run_named` (the slice), `materialize_args`, `system_info`, `eval`, `format_term`, and `eval_closures` (the `-eval` fun store shared with `RuntimeContext::apply_fun`)
* `src/main.rs` → the thin executable shim over `cli::run`
* `tests/slice.rs` → the end-to-end library vertical slice over the real `.beam` fixture
* `tests/cli.rs` → the executable end-to-end (spawns `CARGO_BIN_EXE_beam_rt`)
* `tests/eval.rs` → the `-eval` end-to-end: all 17 differential `Eval` probes pinned to exact stdout + exit, plus control-flow extras and the unsupported-classification check
* `tests/multimodule.rs` → the multi-module host test (compiles a fixture with the system `erlc`, calls `lists:reverse/1` + `maps:get/2` + `io:format/2` + the on-demand stdlib `ordsets`; skips loudly without a system stdlib)
* `../beam_engine/src/context.rs` → the `ExecutionContext` seam this crate implements (read it; do not change)
* `../beam_bifs/` → the `BifRegistry`/`BifContext`/sinks this crate drives (read-only)
* `../beam_loader/tests/fixtures/slice.beam` → the shared oracle fixture reused by the slice tests (read-only)
* `../../erts/emulator/beam/erl_bif_info.c`, `../../erts/etc/common/erlexec.c`, `../../erts/emulator/sys/unix/sys.c`, `../../erts/emulator/beam/erl_init.c` → A.6/A.7/A.8 ground truth (read-only)

## Handoffs (the remaining non-agreeing cases are owned by OTHER nodes)
`differential suite` is at **194 agree / 1 mismatch / 2 unsupported** (of 197). No non-agreeing case
has its root cause in this node; recorded so the next pass routes directly:
* `driver/port_backed_by_spawn_driver` (A.4) — `erlang:open_port/2` (`{spawn,"true"}`) is not a BIF, so
  the Rust side exits 1 `exception error 'erlang:open_port/2 is not a BIF'` where the oracle prints
  `ok true`. Ports are **`beam_bifs`**/**`beam_io`** territory (`beam_io` owns `Port` + built-in
  drivers).
* `semantics/binary_comprehension` and `semantics/bit_size_of_bitstring` (A.1) — **by design** in this
  node: the `-eval` grammar has no binary comprehension and cannot enter a non-byte-aligned binary
  segment literal, so each is an honest exit-3 `beam_rt: unsupported: …` (see Known Issues), never a
  wrong result.
* **`beam_engine` — name the rejected opcode.** Kernel `code.beam`'s `code_server:call/1` executes `recv_marker_reserve/1` (also `recv_marker_bind`, `send`, `loop_rec`, `remove_message`), which no family claims; it falls to the generic `Err(error::unsupported("widened opcode not yet executed"))` at `beam_engine/src/families/mod.rs:93`, which does **not** name the instruction (`Instr::Generic` already does at `interpreter.rs:366`). The three `boot/*` cases that used to surface this now agree via the native `code:*` floor, so this is a diagnostic-quality ask, not a gate blocker.

## See Also
* `../CONTEXT.md` → crate-layer contracts, the seam types, the wave-6 position, the M1 scope table.
* `../../CONTEXT.md` §1 A.6/A.7/A.8 → CLI/configuration, preloaded/boot, introspection contracts; §3 error strategy; §7 staging.
* `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.6–A.8 → the CLI/config, OTP-app and introspection surfaces.
* `../beam_engine/CONTEXT.md` (the seam), `../beam_bifs/CONTEXT.md` (the BIF registry), `../beam_loader/CONTEXT.md` (the IR + fixture).
