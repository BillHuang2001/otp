# `beam_rt` — Rust ERTS Runtime Binary (boot · CLI · `system_info` · runtime glue)

## Intent
The **top of the workspace**: this crate produces the `beam_rt` executable and is the only crate that sees the whole picture. It owns the parts of ERTS no subsystem crate can own alone:

* the **executable entry point** (`main.rs`) and the **CLI flag surface** — contract **A.6**;
* the **boot sequence** and the **preloaded-module registry skeleton** — contract **A.7**;
* **`system_info/1`** and the runtime introspection surface — contract **A.8**;
* the **concrete `ExecutionContext` glue** that adapts the engine's seam types onto the real `beam_bifs` registry / `beam_alloc` heap / `beam_process` pid (the key deliverable);
* the **A.4 runtime side of ports**: `erlang:open_port/2`'s `Name`/`Options` term decoding and the `port_info/1` metadata, over `beam_io`'s one OS-child implementation (`beam_io::drivers::SpawnDriver`), its port registry and its poller (`src/port.rs`). The child and its lifecycle are `beam_io`'s; this crate owns only the terms.

**This crate is the vertical slice.** The engine-level slice runs on a test-only `MinimalContext` (`../beam_engine/tests/slice.rs`); `beam_rt` makes the *same* thing happen through the real runtime glue and the executable end to end. `Runtime::run_function` / `Runtime::run_module` are the evidence.

It depends on **all** other crates (wave 6, `../CONTEXT.md`) but must **call down into them, never duplicate their logic**: BIFs from `beam_bifs`, terms/heaps from `beam_term`/`beam_alloc`, pids from `beam_process`, modules from `beam_loader`, execution from `beam_engine`.

Legacy C ground truth (read-only, never modify): `../../erts/emulator/sys/unix/sys.c` (`main`), `../../erts/etc/common/erlexec.c` (the `erl` flag wrapper), `../../erts/emulator/beam/erl_init.c` (boot), `../../erts/emulator/beam/erl_bif_info.c` (`system_info` keys), `../../erts/preloaded/src/erlang.erl`.

## API Surface (FROZEN — the runtime façade dependents and tests design against)
Re-exported at the crate root and via `beam_rt::prelude::*`.

### `runtime.rs` — the façade
```rust
pub struct Runtime { /* config, registry, code_server, processes, mailboxes, preloaded, bif_heaps, scheduler,
                        runner_state, arena, info_heap, eval_heap, tables, ports, nifs, self_pid, booted,
                        halt, refs, references, eval_closures */ }

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Result<Runtime>;
    pub fn with_defaults() -> Result<Runtime>;                 // RuntimeConfig::default(); booted on demand
    pub fn boot(&mut self) -> Result<()>;                       // the A.7 boot sequence (idempotent)
    pub fn is_booted(&self) -> bool;
    pub fn config(&self) -> &RuntimeConfig;
    pub fn load_module(&mut self, path: impl AsRef<Path>) -> Result<&Module>;   // load `.beam` + cache it
    pub fn module(&self, name: &str) -> Option<&Module>;       // a cached module by name
    pub fn preloaded(&self) -> &PreloadedModules;             // the A.7 registry skeleton
    pub fn refs_mut(&mut self) -> &mut RefAllocator;          // THE reference id space (`make_ref` + monitors)

    pub fn run_function(&mut self, path: impl AsRef<Path>, function: &str, arity: u32,
                        args: &[Eterm]) -> Result<Eterm>;     // THE SLICE (stdout); returns x(0)
    pub fn run_module(&mut self, module: &Module, function: &str, arity: u32, args: &[Eterm],
                      out: &mut dyn OutputSink) -> Result<Eterm>;

    // spawning + scheduling (the single-threaded process runner)
    pub fn spawn_process(&mut self, module: &str, function: &str, args: Vec<Eterm>) -> Result<ProcessId>;   // x(0)→process_result
    pub fn spawn_fun_process(&mut self, fun: Eterm, args: Vec<Eterm>) -> Result<ProcessId>;   // the `spawn/1` fun path
    pub fn enqueue_process(&mut self, pid: ProcessId) -> Result<()>;   // idempotent
    pub fn run_one_slice(&mut self) -> Result<u64>;
    pub fn run_slices(&mut self, max_slices: u64) -> Result<u64>;
    pub fn run_scheduler(&mut self) -> Result<u64>;            // never hangs; send→wake→resume in one call
    pub fn process_result(&self, pid: ProcessId) -> Option<Eterm>;   // x(0) once it returned
    pub fn process_exit(&self, pid: ProcessId) -> Option<Eterm>;     // the reason term once it raised
    pub fn run_queue_len(&self) -> usize;

    pub fn system_info(&mut self, key: &str) -> Result<Eterm>;         // A.8, on the scratch heap
    pub fn system_info_value(&self, key: &str) -> Result<InfoValue>;   // heap-independent source of truth
    pub fn eval(&mut self, expr: &str) -> Result<Eterm>;               // the `-eval` evaluator (A.6)
    pub fn format_term(&mut self, term: Eterm, module: Option<&Module>) -> Result<String>;   // the ~p printer
}
```

### `runner.rs` — the `beam_process::ProcessRunner` (the scheduler's execution seam)
```rust
pub(crate) struct ProcessEntry { module: Arc<Module>, function: Atom, arity: u32 }
struct ProcessEngine { machine: Machine, entry: ProcessEntry, args: Vec<Eterm>, parked: Option<(u32,u32)> }
pub(crate) struct RunnerState { /* engines + results: HashMap<ProcessId,Eterm> + exits: HashMap<ProcessId,Eterm> */ }
impl RunnerState { fn spawn(&mut self, pid, entry, args); fn contains(&self, pid) -> bool;
                   fn result(&self, pid) -> Option<Eterm>; fn exit(&self, pid) -> Option<Eterm>; }
pub(crate) struct RuntimeRunner<'a> { runtime: &'a mut Runtime, state: &'a mut RunnerState }
impl beam_process::ProcessRunner for RuntimeRunner<'_> {
    fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, budget: u32) -> Result<beam_process::RunOutcome>;
}
// run() maps: Returned(v) → record `v`, `Exit(normal)`;  Wait{func,pc} → park + `Waiting` + `Wait`;
//   Err(e) → record `exit_reason(e)`, `Exit(<reason term>)` (never panics). `budget` is accepted but not
//   enforced (no reduction pre-emption ⇒ `Yield` never returned).
```

### `context.rs` — the concrete `ExecutionContext` (the glue)
```rust
pub struct RuntimeContext<'a> { /* runtime: &'a mut Runtime, output: Option<SharedSink<'a>> */ }
// pub(crate) SharedSink<'a>: a cloneable, `Rc<RefCell<..>>`-shared output handle (stdout when None).

impl<'a> RuntimeContext<'a> {
    pub fn new(runtime: &'a mut Runtime) -> Self;
    pub fn with_output(runtime: &'a mut Runtime, sink: &'a mut dyn OutputSink) -> Self;
    pub fn heap_available(&self) -> usize;
    pub(crate) fn eval_heap(&mut self) -> &mut beam_alloc::Heap;   // the long-lived `-eval` heap
    pub(crate) fn run_export(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm>;
    /// Runs **as `pid`**, resolving its process/mailbox against the caller's `table` — the scheduler seam.
    pub fn for_process(runtime: &'a mut Runtime, table: &'a mut ProcessTable, pid: ProcessId) -> RuntimeContext<'a>;
    pub fn pid(&self) -> ProcessId;    // the pid this context runs as (mailbox owner / `self/0`)
}
impl ExecutionContext for RuntimeContext<'_> {
    // call_bif  → try_native (the floor, `src/native/`) → `BifRegistry::contains` → `call` (a `BifContext`
    //             over the shared atom handle / pooled heap / self_pid / output / PendingInterner + a
    //             `DictBridge` OR a `FunBridge` + `RuntimePorts` + the tables unless the call applies a fun
    //             argument); else module=="erlang" → `error::is_known_unimplemented_bif` ? `unsupported:` :
    //             bare `error::undef`; else `find_module_file` → `code_server.load` → `run_export`.
    // apply_fun → external `fun M:F/A` via `call_bif`; `-eval` closure (EVAL_FUN_UNIQ) via
    //             `eval::apply_eval_closure`; else the module's `FunDescriptor{uniq,index}` over
    //             `code_server.modules()` then `Machine::run_label`.
    // ensure_heap → `heap_available() >= heap_need` else `BeamError::SystemLimit` (no GC).
}
```
`call_bif` builds the `BifContext` with a **`PendingInterner`** (a BIF that must create a *new* atom — `binary_to_term`/`list_to_atom`/`binary_to_atom` — records the name; the runtime commits the recorded names into the **one shared table** in order right after the call, at the exact predicted indices, since interning is append-only and single-threaded).
It installs the **`FunCaller`** (`FunBridge`, so the `fun`-taking `lists:*`/`maps:*` BIFs apply their argument via `apply_fun`) and, for the five process-dictionary BIFs, a **`DictBridge`** instead (the two need the process table and cannot co-exist).
This is the **only** place the engine's seam meets the Stage-5 core; it keeps the Stage-1/Stage-5 DAG acyclic (`../CONTEXT.md`).

### `port.rs` — the `PortHook` provider (contract A.4)
```rust
pub const PORT_INFO_ATOMS: &[&str];                        // the `port_info` item vocabulary (seeded at boot)
pub(crate) const PORT_POLL_SLICE: Duration;                // one poll round's budget (25 ms)
pub enum PortName { Spawn, SpawnExecutable, Fd }           // the accepted Name forms / backing driver
pub enum StreamType { List, Binary }                       // `list` (default) / `binary`
pub struct PortRegistry { /* poller: beam_io::Poller, registry: beam_io::PortRegistry,
                             meta: HashMap<u64, PortMeta>, heap: Heap */ }
impl PortRegistry {
    pub fn new() -> Result<PortRegistry>;
    pub fn is_live(&self, id: PortId) -> bool;
    pub fn contains(&self, id: PortId) -> bool;
    pub fn write_command(&mut self, id: PortId, bytes: &[u8]) -> Result<()>;   // `erlang:port_command/2,3`
    pub fn len(&self) -> usize;                            // live-port count (`system_info(port_count)`)
    pub fn is_empty(&self) -> bool;
    pub fn ids(&self) -> Vec<PortId>;                      // LIVE ports' ids, port-number order (`erlang:ports/0`)
    pub fn close(&mut self, id: PortId) -> Result<()>;
    // private: register(driver, meta), pump(timeout) -> Vec<PortEvent>, take_due_exit_statuses, info
}
pub struct RuntimePorts { /* Rc<RefCell<PortRegistry>> + Rc<AtomTable> handle + the owning ProcessId */ }
impl RuntimePorts { pub fn new(registry: Rc<RefCell<PortRegistry>>, atoms: Rc<AtomTable>, owner: ProcessId) -> RuntimePorts; }
// impl beam_bifs::PortHook for RuntimePorts — open_port/2, port_close/1, port_info/1,2
// PortMeta (private): name, command, os_pid, stream, notify_exit, links, monitors, owner
// PortEvent (private): Data { port, bytes, stream, owner } | Exited { port, status, owner }

impl Runtime {
    pub(crate) fn deliver_port_messages(&mut self, table: &mut ProcessTable, timeout: Duration) -> Result<usize>;
    // polls the live ports; DELIVERS each {Port,{data,..}}/{Port,{exit_status,..}} into the OWNING
    // process's mailbox (`deliver_simple`); returns the count (0 when no port is live — never sleeps).
}
impl RuntimeContext<'_> {
    pub(crate) fn poll_port_messages(&mut self, timeout: Duration) -> Result<Vec<Eterm>>;   // the `-eval` inline path
    pub(crate) fn port_data_message(&mut self, port: PortId, bytes: &[u8], stream: StreamType) -> Result<Eterm>;
    pub(crate) fn port_exit_message(&mut self, port: PortId, status: i32) -> Result<Eterm>;
}
```
`PortRegistry` wraps `{ poller, registry, meta, heap }`: **one** long-lived `beam_io::Poller` (a port registers its descriptors when its driver starts), **one** `beam_io::PortRegistry` (id authority + live ports/drivers), the `port_info` metadata `beam_io` does not expose, and a pointer-stable term arena.
`Runtime` owns it as an `Rc<RefCell<PortRegistry>>` (the ETS-tables pattern); `call_registered` builds a `RuntimePorts` from that handle + the shared atom handle + the **owning pid**, so `open_port/2`/`port_close/1`/`port_info/1,2` reach a real OS process without a `&mut Runtime` borrow.
`beam_io::drivers::SpawnDriver` is the **single** OS-child implementation; this crate owns only the **Erlang term decoding** of `open_port/2`'s `Name`/`Options` (into a term-free `beam_io::SpawnSpec`/`SpawnOptions`) and the `PortMeta` — the driver, the option semantics and the read-back gaps are in the ports Known-Issues entry.
`PortRegistry::pump` is the single message producer (polls in `PORT_POLL_SLICE`, runs scheduled port tasks, reports `PortEvent::{Data, Exited}`), consumed by the **scheduler path** (a blocked compiled `receive` is woken by `beam_process`'s deliver + `Scheduler::resume_woken`) and by the **`-eval` `receive … after T`** path (`poll_port_messages`, messages returned inline).

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
pub const SUPPORTED_KEYS: &[&str];                              // the 13 answered keys
pub const INFO_ATOMS: &[&str];                                  // the atom vocabulary the surface needs seeded
pub enum InfoValue { Int(i64), Str(String), Atom(&'static str), Bool(bool), Tuple(&'static str, &'static str) }
impl InfoValue { pub fn to_eterm(&self, heap: &mut Heap, atoms: &AtomTable) -> Result<Eterm>; }
pub fn system_info_value(key: &str, rt: &Runtime) -> Result<InfoValue>;

// format.rs
pub fn term_to_string(registry: &BifRegistry, heap: &mut Heap, atoms: &AtomTable,
                      self_pid: ProcessId, term: Eterm) -> Result<String>;
```

## The runtime-native BIF floor (`src/native/`)
The floor is the set of BIFs that are really *runtime* operations — they read or write the runtime's own state (its process/port tables, code server, atom table, application view, parsed command line) — served **before** the `beam_bifs` registry from `RuntimeContext::try_native` (a directory module; use `try_native`'s dispatch table as the source of truth for what is native):

* `src/native.rs` — module docs, `try_native`, the shared helpers (`string_list`/`atom_name`/`atom`/`charlist`/`render`/`write_output`/`chars_to_string`), plus the `erlang`-core entries `halt/0,1`, `display/1`, `io:put_chars/1,2`, `erlang:apply/2,3`, `erlang:system_info/1`, `os:cmd/1`.
* `src/native/init.rs` — `init:get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`, `get_status/0` (→ `{starting, started}`).
* `src/native/code.rs` — `code:get_path/0`, `all_loaded/0`, `which/1`, `is_loaded/1` (→ `{file, Path}` / atom `preloaded` / atom `false`; a module loaded from bytes with no recorded path reports the empty string `""`, oracle-verified), `is_sticky/1` (bool) + `is_sticky/2` (an `error:undef` raise — the release has no `/2`), `ensure_loaded/1` (→ `{module, Mod}` / `{error, nofile}`), `lib_dir/0,1`, `root_dir/0` (the parent of `lib_dir/0`).
* `src/native/process.rs` — the `erlang` process/registry/reference entries: `self/0` (the **running** pid), `make_ref/0`, `monitor/2` + `demonitor/1,2` (real, over `beam_process`'s monitor machinery + `Runtime::refs_mut`), the spawn family `spawn/1,2,3`, `spawn_link/1,2,3`, `spawn_opt/2,3,4` (a fun entry or an `M:F/A` export), `ports/0`, `processes/0`, `pre_loaded/0` (the oracle's **exact** list *and* order), `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`. `whereis(init)` is the runtime's own root process (oracle `<0.0.0>`).
* `src/native/port.rs` — `port_command/2,3` (writes to the child's `stdin`; binary / char list / nested iolist; `[force]` → `error:notsup`; `{fd, N}` is a typed capability gap) and the `port_info/1,2` item mapping (`undefined` for an unknown/closed port).
* `src/native/application.rs` — `application:get_env/1,2,3`, `get_all_env/1`, `get_key/2`, `loaded_applications/0`, `which_applications/0`, over a minimal native app view (the `.app` files resolved through the `CodeServer` + a recursive-descent `.app` term-text parser).
  **Honest deviation:** `get_all_env(kernel)` reports the `.app` file's **declared** env only — seven pairs (oracle: nine; the two extra, `logger` and `connect_all`, are injected by kernel's own app start at run time and are *not* fabricated). The declared pairs agree key-for-key; only their order differs. `erts` is deliberately not reported loaded.
* `src/native/test_support.rs` — the floor's shared test fixtures.

`beam_bifs` owns the *registry* surface (`bif.tab`: arithmetic, comparison, type tests, term ops, `lists`/`maps`/`binary`, `io:format`, `ets`, …); a native entry must never duplicate one. A call the floor does not claim returns `None` and falls through to the registry unchanged.

### `code:` is the native floor, not a live `code_server`
`code:*`, `init:*` and `application:*` are **preloaded-module surface** (contract A.7): the oracle serves them from `init.beam`/`code.beam`/`application.beam`, but those modules are written against process/message machinery Milestone-1 does not run (for `application` that is the `application_controller`/`application_master` pair + a group leader).
`beam_rt` deliberately ships **no** live `code_server.beam`: OTP's real one needs `monitor/2`+`demonitor`, `register`/`whereis`/`process_flag(trap_exit)`, `spawn_link`, a working `send`, a host **timer** for `after T`, `make_ref/0`, named-ETS resolution, `persistent_term:*`, `filename:*`, `erl_prim_loader:*` and `pre_loaded/0` — all outside this crate — and its *state* already lives in `CodeServer`. The `boot/*` differential cases touching `code:*` are **shape-only** (`is_list`/`is_atom`) and already satisfy.
The wire a future live server must reproduce is the `{code_call, From, Req}` → `{code_server, Reply}` pair (the OTP `code_server:call/1` protocol; `beam_engine` currently rejects `code_server:call/1`'s `recv_marker_*`/`send` by opcode). **`src/native/code.rs`'s `code:*` behaviour must not change** — it keeps the currently-agreeing cases green.

### `erlang`-namespace fallback and the known-unimplemented list
`call_bif` consults the floor, then the `beam_bifs` registry, and only then falls back for the `erlang` namespace. A **known** OTP `erlang` BIF this runtime has not implemented (`error::is_known_unimplemented_bif`) raises the capability-gap `unsupported: erlang:X/Y …` (exit 3); any other unclaimed name raises the bare `error:undef` the oracle does — `error::undef()` is the single constructor, used by `run_export`, `call_bif`, `codeserver` not-found and `spawn_process` alike.
The **effective** gap set is the curated static list (`error::KNOWN_UNIMPLEMENTED_ERLANG_BIFS`, `error::listed_as_unimplemented`) **intersected with what the live registry does not own** — computed at use time, so a later `beam_bifs` change that registers a listed name can never shadow the registry or turn a served BIF into an `unsupported` gap (e.g. `erlang:phash/2`, `phash2/1,2` are registered and therefore not classified here). The drift guard is `error::tests::the_effective_gap_set_never_shadows_the_registry`.

## CLI subset (contract A.6 — the *exact* supported surface; anything else is *unsupported*, exit 3)
| Switch | Meaning in `beam_rt` |
|---|---|
| `-version`, `--version` | print the version banner (OTP `30.0-rc0` / erts `17.1`) and exit 0 |
| `-noshell` | run head-less (M1 default; accepted, no interactive shell exists) |
| `-eval Expr` | evaluate a **general Erlang expression** (`erl_eval`-style) and print **nothing** — the result is discarded, exactly like real `erl`; one trailing `.` is tolerated and stripped |
| `-s Mod Func [Args...]` | apply `Mod:Func(a, b, …)` (Arguments are **atoms**); the result is **discarded**; exit 0 on success |
| `-run Mod Func [Args...]` | same, but Arguments are plain **strings**, materialized as Erlang char lists; the result is **discarded** |
| `-pa Dir` | add `Dir` to the code path (searched **before** `-pz` dirs) |
| `-pz Dir` | add `Dir` to the code path (searched after) |
| `+S N[:N]` | normal scheduler count → `emulator.schedulers` (first number); M1 runs single-threaded |
| `+A N` | async-thread pool size → preserved verbatim in `emulator.extra_emulator_flags` |
| `+P N` | max process count → `emulator.max_processes` |
| `-name N` / `-sname N` | node name → `emulator.node_name` (distribution M1 = skeleton) |
| `run <file.beam> <mod>:<func>/<arity> [int-arg…]` | **the vertical slice**: load the file, run, **print** the result |

* `-eval` grammar (`src/eval/{parse,ast,mod}.rs`): the Erlang **expression** subset — literals in every base, lists/tuples/maps/binaries, the full operator set with Erlang precedence, `case`/`if`/`begin`/`try`/`catch`/`receive`, `fun`s, and list/map/**binary** comprehensions (incl. a **binary generator**, `<<Seg || <<X>> <= Bin>>`). An `erl_eval`-style tree-walk evaluates it, dispatching arithmetic/comparison/**bit operators** (`band`/`bor`/`bxor`/`bsl`/`bsr`/`bnot`, arbitrary-precision) to the **`beam_bifs` `erlang` registry** (only `and`/`or`/`xor`/`not`/`andalso`/`orelse`/`++`/`--` remain special forms).
  Binary segments accumulate a bit buffer, so construction need not be byte-aligned (`<<1:1>>`, `<<A:1,B:7>>`, `<<255:12/little>>`); a byte-aligned result is a **binary**, a non-byte-aligned one a **bitstring** materialized by `beam_codec::term::make_bitstring` on the runtime's long-lived `eval_heap`.
  A map literal is built with the sorted `beam_engine::terms::make_map` (term order). An expression outside the grammar is a **capability gap** (exit 3), not a genuine error.
* **Every command-line flag is an init argument.** Each recognized switch *and* each unrecognized `-flag`/`+flag` (name = the flag with the leading `-`/`+` run removed; values = the following non-flag tokens) is recorded as an `InitArgs` occurrence — the source of `init:get_argument/1`, e.g. `-noshell` ⇒ `{ok,[[]]}`, `-pa /a -pa /b` ⇒ `{ok,[["/a"],["/b"]]}`. An unmodeled switch is **data, not a usage error** (real `erl -noshell --nope -eval 'halt(0).'` exits 0). Only a **stray positional argument** (outside `run`) is a usage error (exit 2).
* `run` argument grammar (each `ARG` token): a **signed integer** (`42`, `-3`) → `ArgTerm::Int`; a **double-quoted string** (`"a\n"`, escapes `\n \t \r \\ \"`) → `ArgTerm::Str` (an Erlang char list); a **list** (`[1, atom, "x", [2]]`) → `ArgTerm::List`; otherwise a bare atom name → `ArgTerm::Atom`. The materialized count must equal the arity (else exit 2). A `-`/`+`-prefixed non-integer token is a usage error. `run`, `-s` and `-run` resolve modules through `code_paths` (`-pa`/`-pz`) + the system stdlib and the file argument (M1 searches for `<Mod>.beam`).

## Exit codes & the `unsupported` signal (the stable external contract)
The CLI exposes **four** exit codes; the differential harness keys off this mapping.

| Code | Meaning | stderr |
|---|---|---|
| `0` (`EXIT_OK`) | success | — |
| `1` (`EXIT_RUNTIME`) | genuine runtime error (a load/execute/BIF failure, e.g. a missing `.beam`, an `undef`, BIF `system_limit`) | `beam_rt: <error>` |
| `2` (`EXIT_USAGE`) | malformed invocation / usage error (a missing operand of a *known* switch, a malformed `run` spec, a stray positional argument, a malformed numeric switch value) | `beam_rt: <error>` then [`USAGE`] |
| `3` (`EXIT_UNSUPPORTED`) | **unsupported** — a Milestone-1 capability gap | exactly one line: `beam_rt: unsupported: <reason>` (no [`USAGE`]) |

Unsupported is emitted for a `-eval` expression outside the grammar (`src/eval/parse.rs`), a BEAM-engine capability gap (a raise whose reason atom starts with `unsupported_opcode`), or a `beam_bifs` capability gap (reason atom `unsupported_bif`).
A capability gap is **uncatchable**: `src/eval/mod.rs`'s `flow_from_error` turns it into `Flow::Fatal` (like `halt`), so the self-terminating `try (EXPR) of … catch C:R -> … end` probe the harness wraps expressions in **cannot** swallow it and print it as an apparent `exception error …`; it escapes to `cli::run` as the exit-3 line — an unimplemented feature is honestly `unsupported`, never a false `error`.

**Classification convention.** A capability gap is signalled by a raise whose **reason atom starts with `unsupported`**. `src/error.rs::unsupported_reason` is the **single** place that convention is interpreted (the constructors there — `unsupported`/`bad_eval` — and the sibling crates' `unsupported_opcode: <what>` / `unsupported_bif` all follow it); it strips the leading `unsupported` token and any following `:`/`_`/space (`unsupported: X`→`X`, `unsupported_opcode: Y`→`opcode: Y`) and returns `Some(reason)`. Classification must go through `unsupported_reason` (which reads `ExitReason::as_atom`) rather than `BeamError::as_atom`, because the latter returns `None` for atoms backed by an **owned** `String` — which every message here is.

**Structured raise reasons are rendered as the real term.** A raise may carry an opaque live reason whose payload is a `beam_term::Eterm` (the `beam_common::Raise` convention). `beam_rt` recovers it in `src/error.rs::live_reason_term` and renders it through the one printer (`Runtime::format_term_w`): `src/runner.rs::exit_reason` uses it as the process's exit reason and `src/cli.rs::run_file` (via `reported_reason`) for the CLI diagnostic; an escaping raise in `src/context.rs::run_export` also passes through `error::name_atom_raise`, which recovers a bare-atom reason so it is still classified by name. So `beam_rt run <m>.beam m:badkey/0` → `beam_rt: error {badkey,nokey}` (exit 1), never a placeholder.

## `system_info/1` keys (contract A.8)
`InfoValue` is the heap-free source of truth; `Runtime::system_info` materializes it on the scratch heap. Strings are Erlang **strings** (lists of char small-ints) exactly as the oracle returns them (`erlang:system_info(version)` → `"17.1"`, verified against OTP 29/erts-17.1).

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
| `port_count` | `Int` — the number of **live** ports (`PortRegistry::len`) |
| `os_type` | `Tuple("unix","linux")` (or `("win32","nt")`) |
| `threads` | `Bool(true)` → atom `true`/`false` |
| `emu_flavor` | `Atom(rt.config().emulator.flavor.as_str())` → `jit` by default (the baseline JIT), `emu` for a `Flavor::Emu` config |
| `schedulers` | `Int` — an explicit `+S N` wins; `+S 0`/unset falls through to `available_parallelism()`; always `> 0` |
| anything else | `Err(BeamError::BadArg)` — matches the oracle's `error:badarg` |

`INFO_ATOMS` (`src/info.rs`) is the complete atom vocabulary of the surface (`unix,linux,win32,nt,true,false,jit,emu`); `seed_core_atoms` (`src/boot.rs`) interns it, because `InfoValue::to_eterm` resolves atoms **through the runtime's atom table** and reports `BeamError::TermNotFound` rather than inventing a term. Add any new atom-valued key's atom to `INFO_ATOMS`.

## Constraints
* **Errors are return-based, workspace-wide** (`../CONTEXT.md` §3): every fallible API returns `beam_common::Result`. **No crate-local error enum**, **no `panic!`/`unwrap()` on runtime data**, **no silent swallowing**. CLI diagnostics are carried as `BeamError::Raise(Raise::error(ExitReason::atom(msg)))` (`BeamError: Display` renders it).
* **`unsafe`-free.** The whole crate is `#![deny(unsafe_code)]`: the glue uses only the safe peripheries of the lower crates (`Heap::cons/tuple`, `AtomTable::intern`, `BifRegistry::call`).
* **Call down, never duplicate.** BIFs → `beam_bifs`; term build/inspect → `beam_term`; heap → `beam_alloc`; `.beam` → `beam_loader`; execution → `beam_engine`; pids/process table → `beam_process`. The `~p` printer is **not** re-implemented — `format.rs` drives the registered `io:format/2` BIF.
* **Frozen term layout** (`../CONTEXT.md` §2): terms are built/inspected only through `beam_term`.
* **Files ~1000 lines**, split by responsibility. `#![warn(missing_docs)]`; every public item documented.

## Design Decisions
* **One shared atom table (the code server's).** There is **no** separate module-local/global split: `CodeServer` owns **one** `AtomTable` (in an `Rc`, copy-on-write on intern), every module it loads is interned into it, and `call_bif` hands `beam_bifs`' `BifContext` that same table — so an atom a BIF builds is an index the engine compares against and the printer names, for **every** loaded module. Indices are assigned in insertion order and never renumber.
* **Heap lifetime.** `Runtime` **owns** the process heap so a returned `Eterm` stays valid after `run_*` returns (the `RuntimeContext` only borrows it). `system_info`/`format_term` use a separate `info_heap`, `reset()` at the start of each call — a term they return is valid **until the next such call**.
* **`ensure_heap` without GC.** M1 has no engine collector, so `ensure_heap` checks capacity and returns `BeamError::SystemLimit` when short; the default `heap_words` (64 Ki words) is ample for the slice. Growth-by-collection is Stage 4 (`beam_gc`).
* **CLI is a documented subset, never a silent no-op.** Unmodeled emulator flags are **preserved verbatim** in `emulator.extra_emulator_flags`; a switch outside this crate's documented surface is a **capability gap** reported as *unsupported* (exit 3) naming it, not a usage error. `-eval` and `run` reuse the BIF registry rather than re-implementing arithmetic/printing.
* **`system_info` returns `Eterm` via `InfoValue`.** The value is computed heap-independently and materialized once (`InfoValue::to_eterm`), so the keys are unit-testable without a heap and materialization stays in one place. **`main.rs` is a thin shim** over `cli::run(argv)`.
* **The scheduler's borrow strategy: move the table (and the runner state) out of the `Runtime`.** A pass needs `&mut Runtime` **and** `&mut ProcessTable` at once, and the table is a *field* of the runtime (crate is `#![deny(unsafe_code)]`, may not alias). `Runtime::run_pass` **moves** both out (`take_processes`/`put_processes`, a `std::mem::replace` with a fresh `ProcessTable::new(SignalAtoms::from_table(runtime.atoms_mut()))`), runs the scheduler with those owned values, then puts both back whatever the outcome. The runner also takes the pid's `Machine` **out of** its `HashMap` for the slice and puts it back unless the process finished. `Runtime::scheduler` persists across passes (not moved), so `run_one_slice` followed by `run_scheduler` continues where the slice left off.
* **A process's exit reason is not its return value.** BEAM terminates a normally-returning process with `normal`, discarding `x(0)`; the runner **records** the final `Returned` value per pid in `RunnerState::results` (`Runtime::process_result`) and returns `RunOutcome::Exit(normal)`. The value is first **copied onto the runtime's pointer-stable arena** by `Runtime::stabilize_term` (`beam_message::copy_struct_simple` onto `eval_heap`, never reset) — an immediate is shared unchanged, a container/binary is copied. The copy is load-bearing: a value taken out of a process's mailbox points into that **process's own** heap, dropped the moment it terminates, so recording the pointer would dangle (see `tests/port_wake.rs`). A process that **raises** records the mapped reason term in `RunnerState::exits` (`Runtime::process_exit`) and exits with that very term — never swallowed.
* **The port subsystem: one poller, one `beam_io` registry, `SpawnDriver` — this crate owns only the terms.** See the `port.rs` API section and the ports Known-Issues entry. The state travels as an `Rc<RefCell<PortRegistry>>` because `call_registered`'s `FunBridge` needs `&mut Runtime` at the same time; a nested borrow is a typed capability gap, never a panic.
* **The host seams: `FunCaller` (`FunBridge`) and `DictHook` (`DictBridge`).** `beam_bifs` applies a user fun through `BifContext::call_fun` (default `unsupported_bif`); `beam_rt` installs `FunBridge`, whose `call_fun` delegates to the **existing** `RuntimeContext::apply_fun` — the evaluator is never re-implemented, which makes `lists:map(fun(X) -> X*X end, [1,2,3])` → `[1,4,9]`. The five process-dictionary BIFs (`erlang:put/2`, `get/1`, `erase/1`, `get_keys/0,1`) go through a `DictBridge` over the **running process's** `beam_process::dict` instead. Because either bridge needs `&mut Runtime` while `BifContext` borrows runtime state, **every** `BifContext` resource is a non-runtime-borrowing clone: `Rc<BifRegistry>`, an `Rc<AtomTable>` handle, an `Rc<RefCell<TableRegistry>>`, a **pooled** `beam_alloc::Heap` (`take_bif_heap`/`put_bif_heap`, stack discipline) and the disjoint `output` field; that leaves the bridge as the only `&mut Runtime` borrow in `call_registered`.
* **`nif_error` raises the live reason term.** `beam_bifs`' `erlang:nif_error/1,2` build a `Structured` reason that carries no `Eterm`; `src/eval/mod.rs` therefore intercepts `nif_error/1,2` and raises `Flow::Raise { class: Error, reason: <the argument term> }`, so the **exact** term survives (`nif_error(foo)` → `exception error foo`) and the raise stays catchable. The same term-carrying `Flow::Raise` path is used for `error`/`exit`/`throw` with a live reason.

## Test Strategy
* **Unit tests** (`#[cfg(test)]` per module) cover: CLI parsing (every supported switch, `init_args` recording of recognized *and* unmodeled switches, the `run` argument grammar, the `-eval` trailing-dot, `-run`); `system_info_value` for every key + the unknown-key `badarg`; `InfoValue` → `Eterm`; `PreloadedModules`; the evaluator (`2+3`→5, `2*3+4`→10, `(1+2)*3`→9, `case`/`catch`/`fun`/comprehension, malformed input → typed gap); CLI `+S/+P/-pa/-pz`; the `FunCaller` path (`lists:map(fun(X) -> X*X end, [1,2,3])` → `[1,4,9]`) and `nif_error`'s live reason; the process-dictionary BIFs through `DictBridge`; `error::unsupported_reason` (`Some` for `bad_eval`/`unsupported_opcode`/`unsupported_bif`, `None` for `missing_value`/`unexpected_argument`/`BadArg`/`SystemLimit`), `error::halt_status`, and the `undef`/`live_reason_term`/`name_atom_raise` helpers.
  The binary surface is pinned separately: a **binary comprehension** (`<< <<(X+1)>> || <<X>> <= <<1,2,3>> >>` → `<<2,3,4>>`), a binary generator driving a list comprehension plus a **sized pattern** (`[X || <<X:16>> <= <<0,1,0,2>>]` → `[1,2]`), and **non-byte-aligned construction** asserted through `beam_codec::term::bitstring_bytes` (`<<1:1>>`, `<<255:12>>`, `<<255:12/little>>`, `<<A:1,B:7>>`, a sized `bits` head before a `binary` tail).
* **`tests/slice.rs`** — the end-to-end library slice (the key evidence): the committed fixture `../beam_loader/tests/fixtures/slice.beam` (reused by relative path, never copied) through the **real** `RuntimeContext`/`BifRegistry` — `add/2` → 5, `fib/1 10` → 55, `pick/1` (atom arg).
* **`tests/cli.rs`** — the executable end-to-end (`CARGO_BIN_EXE_beam_rt` + `std::process::Command`): `run <slice.beam> slice:add/2 2 3` → `5`; `slice:fib/1 10` → `55`; `--version` → exit 0 + version; `-eval 2+3` → exit 0 with **empty** stdout/stderr; an **unknown switch** → exit 0 (init argument); `-eval` outside the grammar → exit 3 + one `beam_rt: unsupported:` line and **no** `Usage:`; a missing `.beam` → exit 1; a missing switch operand or a stray positional arg → exit 2 + `Usage:`; `run <m>.beam m:badkey/0` → `beam_rt: error {badkey,nokey}` (exit 1).
* **`tests/eval.rs`** — all **17** differential `Eval` probes at the CLI boundary, with the **same** self-terminating `try … catch` wrapper + argv the harness builds (`-noshell [extra…] -eval <probe>`), pinning each probe's **exact** stdout + exit without the oracle at run time; plus control-flow extras (`case`/comprehension/`fun`/`element`/`abs`, goldens verified against system `erl`), the two `semantics/*` binary-segment probes, and an unsupported-classification check.
* **`tests/multimodule.rs`** — the multi-module host: compiles a fixture with system `erlc`, runs it through `Runtime::run_function` (`lists:reverse/1` → `[3,2,1]`, `maps:get/2` on a map literal → `7`, the on-demand stdlib `ordsets`); **skips loudly** without a toolchain/stdlib.
* **Ports, the port-stream read, nested re-entrancy, bit operators.** `src/port.rs` unit tests: all three Name forms and their options (incl. **`{fd, N}` adoption** of a caller-owned descriptor over a `UnixStream::pair()` end — `os_pid == undefined`, and `port_close/1` releases the port **without** closing the adopted fd), `BadArg` without consuming a port number, close/reap, the seeded `port_info` vocabulary, the `{fd, N}` `port_command` capability gap, `pump`'s once-only `{exit_status, …}`, `ids()` across a released *lower* slot.
  `tests/eval.rs` adds the `driver/port_backed_by_spawn_driver` probe (a real `os_pid`, the `port_info` pairs, `port_close/1`) and the `driver/spawn_driver_port_stream_read` golden (`ok <<104,105,10>>`, the `list` default, the bound-port match, the `after` fallback, the >64-byte binary read as an exit-3 gap); `tests/port_stream.rs` exercises the **same** read path structurally through `Runtime::eval`; `context.rs` covers the nested-BIF seams (`ets_inside_an_applied_fun_reaches_the_outer_table`, `a_nested_io_format_reaches_the_callers_sink`, `a_nested_call_without_a_fun_still_sees_the_outer_tables`); `eval/mod.rs` covers the six bit operators (small-int sanity, a `1 bsl 70` bignum round-trip, a non-integer operand → `badarith`).
* **`tests/port_wake.rs` — the port → mailbox wake proof.** `port_recv.beam`'s `recv/0` (`open_port({spawn,"echo hi"},[binary])` then `receive {Port, {data, Data}} -> Data end`) is spawned and the scheduler driven in a **bounded** loop (`run_scheduler()` per round until `process_result(pid).is_some()`, 10 s deadline, 5 ms sleep — the child starts asynchronously); asserts `terms::binary_bytes(process_result(pid)) == b"hi\n"`, that the process terminated `normal`, left the process table, and the run queue is empty. Nothing ever `send`s to the receiver, so this is the only proof that **port data** drives `deliver_port_messages` → the mailbox → `Scheduler::resume_woken`.
* **The runtime-native floor's unit tests** are colocated in `src/native/*.rs` (fixtures from `src/native/test_support.rs`): the `erlang`-core entries, `init:*` and boot status, every `code:*` entry, the process/reference/monitor/registry entries, and the `application:*` entries + the `.app`-resource parser (system-installation cases skip **loudly** via `system_runtime`). `src/error.rs`'s tests pin the effective gap classifier (a curated entry is a capability gap; the **effective** gap set is disjoint from the registry — the drift guard) plus the `undef` raise.
* **`tests/mailbox.rs`** — the nine engine `Mailbox` seam methods over a real `beam_message::Queue`: enqueue/len/scan/cursor, the `recv_marker_*` table, `send_message`'s `from` reflecting the running pid — pinned against `Runtime.mailboxes` per-pid state surviving a blocked `receive`.
* **`tests/scheduler.rs` — the two-process blocking + resume-on-send proof.** Reuses `../beam_engine/tests/fixtures/recv.beam` (`wait/0` = `receive X -> X end`, `bomb/1` = `Pid ! 42, ok`, `sel/0`, `scan/0`) plus `tests/fixtures/sched.beam` (`send2(Pid,N) -> Pid ! 42, Pid ! {bar, N+7}, ok`). Tests: a receiver + a sender seeded with `ProcessId::to_eterm` → the receiver's result is `42`; run the receiver FIRST (assert `ProcessState::Waiting`) then the sender, asserting the send wakes it; a cross-process selective receive (`sched:send2/2` skips a non-matching `42` head, the `{bar,7}` tail delivers `7`). Oracle values verified against system `erl`. **Caveat:** the fixtures directory must not be passed to the oracle as a single `-pa` — it contains a `lists.beam` that shadows stdlib.
* **Gate:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt` GREEN, and the whole-workspace `build`/`test` stays green. The compatibility gate is the sibling `tools/differential` harness: build this crate in release, then `erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p differential -- suite`.

## Known Issues / Intentional M1 gaps
* **No interactive shell, no distribution, no node.** `-noshell` is the only mode; `-name`/`-sname` are parsed and stored but distribution (A.5) is not started.
* **Preloaded modules are a name-set skeleton.** Boot does **not** load preloaded `.beam`s, and the shared atom table is seeded with `CORE_ATOMS` + `INFO_ATOMS` (not the full `atom.names` ordering), so positional atom indices for preloaded code are not reproduced. The erts app's `ebin` **is** on the code path (`CodeServer::resolve_system_ebins`), so a preloaded module (`init`, `persistent_term`, …) *can* be loaded from disk on demand; the registry (the oracle's **exact** `erlang:pre_loaded/0` list *and* order) is in place.
* **Answers that read *this* runtime's own state differ from the oracle by construction** — not bugs, and must not be "fixed" to a run-time figure: `erlang:processes/0` (this runtime's own table — its root process plus whatever `spawn_process` created, never the oracle's 40-odd background processes), `erlang:ports/0` and `erlang:system_info(port_count)` (the live `PortRegistry`, empty until something opens a port), and `erlang:module_loaded/1` (the preloaded names + the modules this runtime loaded). `erlang:whereis(init)` **does** agree (the root process; oracle `<0.0.0>`). `erlang:registered/0` is not implemented (on the known-unimplemented list).
* **`-eval` results that are not byte-aligned cannot be *printed*.** Construction and `bit_size/1` are supported for a non-byte-aligned bitstring (`<<1:1>>`, `<<255:12/little>>`), but the registered `beam_bifs` `io:format` `~w`/`~p` printer rejects one with `badarg` — a **`beam_bifs` printer gap outside this crate** (the corpus probes read such a value through `bit_size/1`, so they are unaffected). The honest segment limits remain: a construction segment **or** a pattern segment wider than 64 bits is *unsupported*, and a value that needs a bignum is `system_limit`.
* **The `beam_bifs` `~p` printer rejects a container wider than its ~80-column rendering limit.** The term is correct, but printing it *inside a container* that does not fit one line fails: `io:format("~p~n",[code:is_loaded(lists)])` → `beam_rt: badarg` (exit 1) where the oracle prints `{file,"…/lists.beam"}` — another **`beam_bifs` printer gap outside this crate** (a `{file,"…"}` tuple renders fine up to ~60 columns and is `badarg` past ~80). It shows up on `code:*`-shaped native answers because they embed an installation path.
* **Scheduler is single-threaded; a few M1 process gaps.** `spawn_process` + `run_one_slice`/`run_slices`/`run_scheduler` run spawned processes and a blocked receiver **is** resumed by a sender (send→wake→resume via `resume_woken`); the run loop is single-threaded (never a thread). Remaining gaps, all honest: **no reduction pre-emption** (`budget` is not enforced; `Yield` is never returned; a process runs to its next `return`/`wait`); a non-zero `receive ... after T` **suspends (`Wait`) instead of timing out** on the *scheduler/runner* path (no host timer — a message never sent leaves the process `Waiting`; only the `-eval` `receive … after T` path advances, by polling ports); and on the runner path a **capability gap is uncatchable** — a process raising `badmatch`/`case_clause` terminates with the real reason term where carryable (else the mapped atom / `Display` text). `system_info(process_count)` reflects the runtime's own process table entry.
* **No live `code_server`.** `code:*` on the receive path is the native floor in `src/native/code.rs` (see the `code:` section above), not a running `code_server` process.
* **`-eval`/`-s`/`-run` are documented subsets** (`-s` uses atoms + discards the result, `-run` uses strings; `-eval` evaluates the expression subset and prints nothing). An `-eval` expression outside the grammar is *unsupported* (exit 3). The `!` **send operator** and a **bare `receive`** that drives a spawned sender are supported; the remaining M1 expression gaps are a bare `receive` that never receives a message (no sender is runnable ⇒ `unsupported: receive: no matching message arrived`), a `receive` inside a spawned process, and `after infinity` — all typed errors in `src/eval/mod.rs`. A capability gap is **uncatchable** (it travels as `Flow::Fatal` like `halt`, so a `try`/`catch` cannot turn it into an apparent exception) and the CLI exits 3. Real `erl` start-script processing, `-init_debug` and the rest are absent — like any unmodeled switch, recorded as init arguments rather than acted on.
* **A lambda has no faithful `NEW_FUN_EXT` image.** `fun M:F/A` builds a real `FunKind::External` term (`term_to_binary` → `EXPORT_EXT`), but a lambda (`fun() -> ok end`) is an evaluator closure (`EVAL_FUN_UNIQ` + an index into `Runtime::eval_closures`) — a `Local` fun, which the ETF codec cannot encode, so `term_to_binary(fun() -> ok end)` is the honest typed `unsupported: local_fun` (the `wire/new_fun_ext_encode` corpus case). A byte-exact `NEW_FUN_EXT` would need `erl_eval`'s own 16-byte MD5 uniq, fun index, creator pid and expression env — not reproducible without mimicking `erl_eval` internals; no wrong encoding is emitted.
* **A record definition cannot be read.** `records:get_definition/2` is the typed `unsupported` of the `records/get_definition_unknown_record` corpus case: the code-server record-definition table does not exist here (`beam_loader` declares but does not decode the `Recs` chunk). Contract A.7; the `beam_bifs`/`beam_loader` side is outside this crate.
* **Nested-BIF re-entrancy (a `fun` applied inside a BIF) is handled — with one limitation.** When a `fun`-taking `lists:*`/`maps:*` BIF calls back through `FunBridge`, the nested `RuntimeContext` takes a **fresh pooled heap**, shares the caller's sink through a `SharedSink` clone and reaches the runtime's ETS tables: `call_registered` installs the `TableRegistry` **only for a call that cannot apply a fun argument**, so the outer call leaves the shared `RefCell` free for the nested `ets:*` to borrow. The one limitation: a BIF that needed ETS *and* applied a fun argument would not get tables — none exists in M1. Output sharing costs `RuntimeContext` a destructor (the `Rc`-backed `SharedSink`), so a caller that keeps a `RuntimeContext` alive cannot reuse the `Runtime` binding directly until it goes out of scope; read it through the context instead.
* **Ports match the oracle's observable shape.** `open_port/2` really spawns the OS process: `{spawn, Cmd}` runs through `/bin/sh -c "exec Cmd"` (the shell replaces itself, so builtins / `VAR=x` / compound lines behave like the shell and a missing command exits 127 — matching the oracle), `{spawn_executable, Path}` runs the path directly with `{args, …}`, and `{fd, N}` **adopts** the caller's descriptor; the port gets a real `beam_process::PortId` out of `beam_io`'s registry and its metadata is recorded; `port_close/1` kills/reaps the child and releases the id; `erlang:ports/0` reports the live ports.
  **`port_info/1` answers the oracle's seven-item set in oracle order** — `{name, …}`, `{links, [Owner]}`, `{id, N}` (a **bare small integer**), `{connected, Owner}` (the owner pid), `{input, 0}`, `{output, 0}`, `{os_pid, N|undefined}`; `port_info/2` returns `{Item, Value}` and additionally answers `monitors`; `stream` is **not** an item (oracle: `error:badarg`); an unknown/closed port is the atom `undefined`; `input`/`output` are always `0` (no per-port totals are kept). `port_command/3` with `[force]` raises `error:notsup` (the spawn driver carries no `ERL_DRV_FLAG_SOFT_BUSY`); `open_port(_, [list])` raises `badarg` (the whitelist excludes `list` — a list stream is already the default).
  Remaining honest gaps: the child's **stdout** is read back by both port-message producers (scheduler path → the owning process's mailbox, proven by `tests/port_wake.rs`; `-eval` path → inline), but the read is **one** `Read::read` per readiness and **stderr is never read**; `port_command/2,3` writes to the child's stdin and returns `true` (`BadArg` for a non-port or an unknown/closed id), but the `{fd, N}` form is a typed capability gap (no child pipe to write to); `connected` flips to `false` only at `port_close/1`, not when the child exits by itself; `exit_status` is honored (a reap emits exactly one `{Port, {exit_status, Status}}` — `Status` = the exit `code()` else `128 + signal()` — handed out **once** by `pump`); `eof` is inert; a self-exiting child is reaped by a later poll or by `port_close/1`/runtime exit; `{spawn_executable, Missing}` is `BeamError::Io` where the oracle gives `error:enoent`; and a `binary`-stream read wider than 64 bytes is the typed *unsupported* gap. `os:cmd/1` remains the runtime-native host spawn.
* **Heap has no GC** — a runaway allocation returns `SystemLimit`.
* **No memory introspection.** `erlang:memory/0,1`, `erlang:process_info/1,2` and `erlang:statistics/1` are **not** registered BIFs (`beam_bifs` raises `… is not a BIF`, exit 1) and none of the answered `system_info/1` keys reports memory (`SUPPORTED_KEYS`). The registered `ets:info(T, memory)` is the only memory number available.

## Routing Table
* `src/lib.rs` → crate docs, `#![deny(unsafe_code)]`, module wiring, root re-exports, `prelude`
* `src/error.rs` → the `pub(crate)` `BeamError` constructors (missing value, unexpected argument, bad spec, arity mismatch, bad run argument, unknown module/info key, unsupported, bad eval, `undef`), the halt helpers, `unsupported_reason`, `live_reason_term`/`name_atom_raise`, and the curated `KNOWN_UNIMPLEMENTED_ERLANG_BIFS` + `is_known_unimplemented_bif` — no crate-local error enum
* `src/config.rs` → `RuntimeConfig`, `Action`, `ArgTerm`, `InitArgs`, `EmulatorConfig` alias, defaults/setters
* `src/cli.rs` → `USAGE`, `parse` (the A.6 parser + the `run` argument grammar), `run` (driver + the four `EXIT_*` codes), `run_file`/`reported_reason` (renders a structured raise's real reason term through `format_term_w`)
* `src/codeserver.rs` → `CodeServer`: the **one** shared `Rc<AtomTable>` (copy-on-write on intern), module cache, code path; a module on no code-path directory raises the bare `error::undef`; `resolve_system_ebins` adds the stdlib, kernel **and erts** `ebin` dirs (the erts app holds the preloaded `.beam`s), resolved at run time (never baked in)
* `src/boot.rs` → `PreloadedModules` skeleton (A.7): `PRELOADED_MODULE_NAMES` is the oracle's **exact** `erlang:pre_loaded/0` list *and* order (`persistent_term` first … `erts_code_purger` last); `CORE_ATOMS` (the core-atom seed, also interns `info::INFO_ATOMS`/`port::PORT_INFO_ATOMS`) whose **last three appended entries are the exception reason/class atoms `function_clause`, `exit`, `throw`** — so an engine `func_info` raise (`error:function_clause`; C: `i_func_info_IaaI`) names its reason even in a module that never mentions the name; plus the boot banner
* `src/port.rs` → the A.4 `PortHook` provider (see the API section + the ports Known-Issues entry): `PortRegistry`, `RuntimePorts`, `Runtime::deliver_port_messages`, the `-eval` `poll_port_messages`/`port_data_message`/`port_exit_message` trio, `PortName`/`StreamType`/`PORT_INFO_ATOMS`/`PORT_POLL_SLICE`. Its production half is **~1175 lines**, past the baseline because of the colocated `#[cfg(test)] mod tests`; **not** restructured
* `src/info.rs` → `SUPPORTED_KEYS`, `INFO_ATOMS`, `InfoValue`, `system_info_value` (A.8 keys), `InfoValue::to_eterm`
* `src/context.rs` → `RuntimeContext`, the concrete `ExecutionContext` glue (see the API section): `apply_fun`, `send_message`, `run_export` (bare `error::undef` for a missing function/export; an escaping raise passes through `error::name_atom_raise`), `eval_heap`, `PendingInterner`, `SharedSink`, `reentrancy_capable`, `FunBridge`, `DictBridge`/`uses_dict` (the `DictHook` for the five process-dictionary BIFs), `call_registered` (installs a `DictHook` **or** a `FunCaller` and **no** `SendHook`, so the registered `erlang:send/2` / `erlang:'!'/2` raise `unsupported_bif`), and the `erlang`-namespace fallback
* `src/native.rs` → the runtime-native BIF floor: module docs, `try_native` (consulted **before** the registry), the shared helpers, the `erlang`-core entries; declares the submodules below
* `src/native/init.rs` → the `init:*` entries (`get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`, `get_status/0`)
* `src/native/code.rs` → the `code:*` entries (`get_path/0`, `all_loaded/0`, `which/1`, `is_loaded/1`, `is_sticky/1,2`, `ensure_loaded/1`, `lib_dir/0,1`, `root_dir/0`) over the `CodeServer` state + the preloaded registry
* `src/native/process.rs` → the `erlang` process/registry/reference entries (`self/0`, `make_ref/0`, `monitor/2`, `demonitor/1,2`, the spawn family, `ports/0`, `processes/0`, `pre_loaded/0`, `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`) over `beam_process`'s tables + the runtime reference id space
* `src/native/port.rs` → the `port_command/2,3` write path (the child's `stdin`; `[force]` → `error:notsup`) and the `port_info/1,2` item mapping (`undefined` for an unknown/closed port)
* `src/native/application.rs` → the `application:*` entries over a minimal native app view (the `.app` files resolved through the `CodeServer` + a recursive-descent `.app` term-text parser). Its production half is ~700 lines — past the baseline because of the colocated `#[cfg(test)] mod tests`; **not** restructured
* `src/native/test_support.rs` → the floor's shared test fixtures (`runtime`/`system_runtime`/`call`/`atom`/`chars`/`string_of`)
* `src/eval/mod.rs` → the `-eval` evaluator: an `erl_eval`-style tree-walk with its own `Flow` control signal (live raise reasons; `halt` **and** a capability gap are uncatchable) dispatching through `RuntimeContext`; the `!` send operator and `receive` (pumping the scheduler via `Runtime::pump_scheduler`); `fun M:F/A` builds a real `FunKind::External` (lambdas stay evaluator closures); map literals use the sorted `beam_engine::terms::make_map`; binary-comprehension evaluation and bit-buffer construction (`BitBuf`/`materialize_bits`); `apply_eval_closure`
* `src/eval/ast.rs` → the `-eval` AST (`Expr`/`Clause`/`Try`/`Seg`/`Qual`/`MapField`/`BinOp`/`UnOp`)
* `src/eval/parse.rs` → the `-eval` lexer + recursive-descent/Pratt parser (Erlang precedence; the `<=` binary-generator qualifier; a segment's `:Size` parsed in segment context so a following `/Type` is a specifier, not a division; malformed input → `error::bad_eval`, exit 3)
* `src/format.rs` → `term_to_string`/`term_to_string_w` — drives the registered `io:format/2` `~p`/`~w` printer
* `src/mailbox.rs` → `RuntimeContext`'s nine `beam_engine::Mailbox` seam methods over the running process's real `beam_message::Queue`, with per-pid scan/marker state in `Runtime.mailboxes: HashMap<ProcessId, MailboxState>` (the receive cursor + the `recv_marker_*` table) — the durable position a blocked `receive` resumes against
* `src/runner.rs` → the `beam_process::ProcessRunner` (see the API section): `ProcessEntry`, `ProcessEngine`, `RunnerState`, `RuntimeRunner`, `exit_reason` (error's own atom → a `Raise`'s atom → the reason's live term → interned `Display` text; never panics); a **fun entry** lets `spawn/1` run a fun body; module docs carry the borrow strategy + the "exit reason is not the return value (and the return value must be stabilized)" rule
* `src/runtime.rs` → `Runtime`: boot-wiring (registers the `init` name on its own root process at construction), code server, module cache, `run_function`/`run_module`/`run_named` (the slice), the **scheduler API** plus the private `scheduler_pass`/`run_pass`/`take_processes`/`put_processes` move-out dance (`scheduler_pass` runs the queue, polls the live ports via `deliver_port_messages`, then `resume_woken`s the woken receivers and loops), `stabilize_term`, `materialize_args`, `system_info`, `eval`, `format_term`, `eval_closures`, `eval_heap`, `spawn_fun_process`, `pump_scheduler`, `refs_mut`; `spawn_process` raises the bare `error::undef` for a missing export; fields `processes` (`pub(crate)`), `mailboxes`, `scheduler`, `runner_state`, `refs`, `references` (the reference→monitor map `demonitor/1` reads), `ports`
* `src/main.rs` → the thin executable shim over `cli::run`
* `tests/slice.rs` → the end-to-end library vertical slice over the real `.beam` fixture
* `tests/cli.rs` → the executable end-to-end (spawns `CARGO_BIN_EXE_beam_rt`)
* `tests/eval.rs` → the `-eval` end-to-end: the 17 differential `Eval` probes, the port + port-stream probes (the `ok <<104,105,10>>` golden), the two `semantics/*` binary-segment probes, the `boot/*` spawn probes, the `erlang:port_command/2` write/echo probe, the `wire/export_ext_encode` golden, control-flow extras and the unsupported-classification check
* `tests/port_stream.rs` → the port-stream read at the **library** boundary (`Runtime::eval`), asserting the `{Port, {data, Data}}` term structurally rather than only through the printer
* `tests/port_wake.rs` → the **port → mailbox wake** proof (only the port poller, never an explicit `send`, drives `deliver_port_messages` + `resume_woken`)
* `tests/fixtures/{mailbox,sched,port_recv}.{erl,beam}` → the committed system-`erlc` oracle fixtures + their regeneration recipe and oracle table (`fixtures/README.md`)
* `tests/multimodule.rs` → the multi-module host test (compiles a fixture with the system `erlc`; skips loudly without a system stdlib)
* `../beam_engine/src/context.rs` → the `ExecutionContext` seam this crate implements (read it; do not change)
* `../beam_bifs/` → the `BifRegistry`/`BifContext`/sinks this crate drives (read-only)
* `../beam_loader/tests/fixtures/slice.beam` → the shared oracle fixture reused by the slice tests (read-only)
* `../../erts/emulator/beam/erl_bif_info.c`, `../../erts/etc/common/erlexec.c`, `../../erts/emulator/sys/unix/sys.c`, `../../erts/emulator/beam/erl_init.c` → A.6/A.7/A.8 ground truth (read-only)

## Handoffs (owned by OTHER nodes)
`differential suite` is at **`summary: total=431 agree=429 mismatch=0 unsupported=2 skipped=0`**. Remaining asks owned elsewhere:
* **The 2 `unsupported` are `records/get_definition_unknown_record`** (contract A.7 — needs the code-server record-definition table; `beam_loader` declares but does not decode the `Recs` chunk) **and `wire/new_fun_ext_encode`** (contract A.5 — a lambda's `NEW_FUN_EXT` image); both are described in Known Issues. Legacy `wire/*` encode/decode asks (`port_info/2` item shape, `PORT_EXT`/`V4_PORT_EXT`, `{minor_version,1}` atom encoding, map-key encode order, `compressed` decode, `string:to_integer/1`'s missing `erts_internal:list_to_integer/2`) live in `beam_codec`/`beam_bifs` and are outside this crate.
* **`beam_engine` — name the rejected opcode.** Kernel `code.beam`'s `code_server:call/1` executes `recv_marker_reserve/1` (also `recv_marker_bind`, `send`, `loop_rec`, `remove_message`), which no family claims; it falls to the generic `Err(error::unsupported("widened opcode not yet executed"))` at `beam_engine/src/families/mod.rs:93`, which does **not** name the instruction (`Instr::Generic` already does at `interpreter.rs:366`). The `boot/*` cases that used to surface this now agree via the native `code:*` floor, so this is a diagnostic-quality ask, not a gate blocker.

## See Also
* `../CONTEXT.md` → crate-layer contracts, the seam types, the wave-6 position, the M1 scope table.
* `../../CONTEXT.md` §1 A.6/A.7/A.8 → CLI/configuration, preloaded/boot, introspection contracts; §3 error strategy; §7 staging.
* `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.6–A.8 → the CLI/config, OTP-app and introspection surfaces.
* `../beam_engine/CONTEXT.md` (the seam), `../beam_bifs/CONTEXT.md` (the BIF registry), `../beam_loader/CONTEXT.md` (the IR + fixture).
