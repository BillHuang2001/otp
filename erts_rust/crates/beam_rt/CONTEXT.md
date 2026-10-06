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
pub struct Runtime { /* config, registry, code_server, processes, mailboxes, preloaded, bif_heaps, scheduler, runner_state, arena, info_heap, eval_heap, tables, ports, self_pid, booted, halt, refs, references, eval_closures */ }

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
    /// The runtime's **one** reference id space — the source of every `erlang:make_ref/0`
    /// reference and every `erlang:monitor/2` reference. Kept separate from
    /// `ProcessTable`'s own allocator so the two can never collide; a term minted here is
    /// recorded in `Runtime::references` (the reference→monitor map `erlang:demonitor/1`
    /// reads).
    pub fn refs_mut(&mut self) -> &mut RefAllocator;

    /// THE SLICE (stdout): load `path` and run `function/arity`; returns `x(0)`.
    pub fn run_function(&mut self, path: impl AsRef<Path>, function: &str, arity: u32,
                        args: &[Eterm]) -> Result<Eterm>;
    /// The general primitive: run against a caller-owned module with an explicit sink.
    pub fn run_module(&mut self, module: &Module, function: &str, arity: u32, args: &[Eterm],
                      out: &mut dyn OutputSink) -> Result<Eterm>;

    // --- spawning and scheduling (the single-threaded process runner) ---
    /// Spawn a process whose entry point is `module:function/arity(args)` and make it
    /// **runnable** (appended to the run queue). The module is resolved through the code
    /// server (load on demand) exactly like `RuntimeContext::run_export`. The `x(0)` is
    /// recorded and readable through [`Runtime::process_result`] once the process runs to
    /// its `return` (a normally-returning process's *exit reason* is `normal`, not that
    /// value — see `runner.rs`/Design Decisions).
    pub fn spawn_process(&mut self, module: &str, function: &str, args: Vec<Eterm>) -> Result<ProcessId>;
    /// Put a spawned process back on the run queue and mark it `Runnable` (idempotent —
    /// an already-queued pid is a no-op; a pid with no engine is a typed error).
    pub fn enqueue_process(&mut self, pid: ProcessId) -> Result<()>;
    /// Run **one** scheduler slice (one process, once) — the step hook; `0`/`1` slices.
    pub fn run_one_slice(&mut self) -> Result<u64>;
    /// Run at most `max_slices` slices, returning how many ran.
    pub fn run_slices(&mut self, max_slices: u64) -> Result<u64>;
    /// Run the scheduler until the run queue empties, returning the slice count. A
    /// process blocked on an empty `receive` simply stays `Waiting` (the loop never hangs);
    /// a send made *inside* a slice wakes the receiver at the slice end (`resume_woken`),
    /// so one call carries send→wake→resume through to completion.
    pub fn run_scheduler(&mut self) -> Result<u64>;
    /// The `x(0)` a spawned process returned, once it ran to its `return` (`None` if it is
    /// still running, blocked, or finished by raising — see [`Runtime::process_exit`]).
    pub fn process_result(&self, pid: ProcessId) -> Option<Eterm>;
    /// The raise reason term a spawned process finished with, once it **raised**.
    pub fn process_exit(&self, pid: ProcessId) -> Option<Eterm>;
    /// The number of processes waiting in the scheduler's run queue.
    pub fn run_queue_len(&self) -> usize;

    /// `system_info/1` (A.8) materialized as a term on the runtime scratch heap.
    pub fn system_info(&mut self, key: &str) -> Result<Eterm>;
    /// `system_info/1` as a heap-independent value (the testable source of truth).
    pub fn system_info_value(&self, key: &str) -> Result<InfoValue>;

    /// Evaluate an Erlang expression (`erl_eval`-style tree-walk; A.6).
    pub fn eval(&mut self, expr: &str) -> Result<Eterm>;

    /// Print a term via the registered `io:format` `~p` printer (no second printer).
    pub fn format_term(&mut self, term: Eterm, module: Option<&Module>) -> Result<String>;

    // ... plus the process-scheduler entry points listed above (spawn/enqueue/run/result accessors).
}
```

### `runner.rs` — the `beam_process::ProcessRunner` (the scheduler's execution seam)
```rust
pub(crate) struct ProcessEntry { module: Arc<Module>, function: Atom, arity: u32 }   // what to run, with which args
struct ProcessEngine { machine: Machine, entry: ProcessEntry, args: Vec<Eterm>, parked: Option<(u32,u32)> }
#[derive(Default)]
pub(crate) struct RunnerState { /* engines: HashMap<ProcessId,ProcessEngine>, results: HashMap<ProcessId,Eterm>, exits: HashMap<ProcessId,Eterm> */ }
impl RunnerState { pub(crate) fn spawn(&mut self, pid, entry, args); pub(crate) fn contains(&self, pid) -> bool;
                   pub(crate) fn result(&self, pid) -> Option<Eterm>; pub(crate) fn exit(&self, pid) -> Option<Eterm>; }
pub(crate) struct RuntimeRunner<'a> { runtime: &'a mut Runtime, state: &'a mut RunnerState }
impl beam_process::ProcessRunner for RuntimeRunner<'_> {
    fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, budget: u32) -> Result<beam_process::RunOutcome>;
}
// run(): takes the pid's ProcessEngine out of the map, builds `RuntimeContext::for_process(runtime, table, pid)`,
//   then `run_export_outcome` (first run) or `resume_outcome` (a parked `receive`, same Machine — no reset).
//   Mapping: Returned(v) → record `v` in `results`, return `RunOutcome::Exit(normal)`;
//            Wait{func,pc} → store `parked`, set `ProcessState::Waiting` via `table.get_mut`, return `RunOutcome::Wait`;
//            Err(e)        → record `exit_reason(e)` in `exits`, return `RunOutcome::Exit(<reason term>)` (never panic).
//   `budget` is accepted for the seam but not enforced (no reduction pre-emption ⇒ `Yield` is never returned).
```

### `context.rs` — the concrete `ExecutionContext` (the glue)
```rust
pub struct RuntimeContext<'a> { /* runtime: &'a mut Runtime, output: Option<SharedSink<'a>> */ }
// pub(crate) SharedSink<'a>: a cloneable, `Rc<RefCell<..>>`-shared output handle (stdout when None),
// covariant in `'a` so a nested context can reuse the caller's sink.

impl<'a> RuntimeContext<'a> {
    pub fn new(runtime: &'a mut Runtime) -> Self;
    pub fn with_output(runtime: &'a mut Runtime, sink: &'a mut dyn OutputSink) -> Self;
    pub fn heap_available(&self) -> usize;
    /// The runtime's long-lived `-eval` heap (a non-byte-aligned bitstring is written
    /// here by `beam_codec::term::make_bitstring`; never reset in M1, so the term is
    /// valid for the runtime's lifetime).
    pub(crate) fn eval_heap(&mut self) -> &mut beam_alloc::Heap;
    pub(crate) fn run_export(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm>;

    /// A context that runs **as `pid`**, resolving that process and its mailbox against the
    /// caller-supplied `table` instead of the runtime's own `processes` — the scheduler-pass seam.
    pub fn for_process(runtime: &'a mut Runtime, table: &'a mut ProcessTable, pid: ProcessId) -> RuntimeContext<'a>;
    /// The pid this context runs as (the mailbox owner and what `erlang:self/0` reports).
    pub fn pid(&self) -> ProcessId;
}
impl ExecutionContext for RuntimeContext<'_> {
    // call_bif  → RuntimeContext::try_native (the runtime-native BIF floor — see the
    //             `src/native/` section below for the full entry list)
    //             → `BifRegistry::contains` → `BifRegistry::call` (a `BifContext` seeded via
    //             `with_fun_caller(FunBridge)` and `with_port_hook(RuntimePorts)` over {pooled heap,
    //             code_server atom handle, self_pid, the tables (only for a call that cannot apply a
    //             fun argument), output, PendingInterner}); else if
    //             module == "erlang" → `error::is_known_unimplemented_bif` ? `unsupported:` :
    //             `error::unknown_bif`; else seed the system code path
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
`call_bif` also installs the **`FunCaller`** (`FunBridge`) on that `BifContext`, so the `fun`-taking
`lists:*`/`maps:*` BIFs apply their `fun` argument by re-entering `RuntimeContext::apply_fun` — see
Design Decisions.
This is the **only** place the engine's seam meets the Stage-5 core; it keeps the Stage-1/Stage-5
DAG acyclic (`../CONTEXT.md`).

### `port.rs` — the `PortHook` provider (contract A.4)
```rust
pub const PORT_INFO_ATOMS: &[&str];                        // the `port_info/1` item vocabulary (seeded at boot)
pub enum PortName { Spawn, SpawnExecutable, Fd }           // the accepted Name forms / backing driver
pub enum StreamType { List, Binary }                       // `list` (default) / `binary`
pub struct LivePort { /* driver name, command, child, stdout: Option<ChildStdout>, os_pid,
                         connected, stream, links/monitors (empty) */ }
pub struct PortRegistry { /* beam_process::PortTable + id → LivePort map + a per-registry scratch Heap */ }
impl PortRegistry {
    pub fn new() -> Result<PortRegistry>;
    pub fn alloc(&mut self) -> Result<PortId>;
    pub fn release(&mut self, id: PortId);
    pub fn contains(&self, id: PortId) -> bool;
    pub fn len(&self) -> usize;                            // the live-port count (`system_info(port_count)`)
    pub fn is_empty(&self) -> bool;
    pub fn ids(&self) -> Vec<PortId>;                      // the LIVE ports' ids, port-number order (`erlang:ports/0`)
    pub fn insert(&mut self, id: PortId, live: LivePort);
    pub fn remove(&mut self, id: PortId) -> Option<LivePort>;
}
pub struct RuntimePorts { /* Rc<RefCell<PortRegistry>> + Rc<AtomTable> handle */ }
impl RuntimePorts { pub fn new(registry: Rc<RefCell<PortRegistry>>, atoms: Rc<AtomTable>) -> RuntimePorts; }
// impl beam_bifs::PortHook for RuntimePorts — open_port/2, port_close/1, port_info/1,2

impl RuntimeContext<'_> {
    /// The port poller: registers every live port's stdout fd with a `beam_io::Poller`, waits up
    /// to `timeout` for readiness, does ONE non-blocking `Read::read` per readable/EOF/error event,
    /// and returns the `{PortId::to_eterm(), {data, Data}}` messages it produced (an Erlang binary
    /// for a `binary` stream, else a char list). `Ok(vec![])` once the timeout elapses with no
    /// readiness; EOF drops the stream handle so a later poll cannot spin on the closed fd.
    pub(crate) fn poll_port_messages(&mut self, timeout: Duration) -> Result<Vec<Eterm>>;
}
```
`Runtime` owns the registry as an `Rc<RefCell<PortRegistry>>` (the same cheap-clone pattern as the ETS
tables) and `call_registered` builds a `RuntimePorts` from that handle + the shared atom handle, so
`erlang:open_port/2`/`port_close/1`/`port_info/1,2` reach a real OS process **without** a
`&mut Runtime` borrow. The diff-harness `driver/port_backed_by_spawn_driver` and
`driver/spawn_driver_port_stream_read` cases AGREE.

The **port stream read** is closed end-to-end: a `spawn`/`spawn_executable` port keeps the child's
`stdout` handle (`LivePort.stdout`), `RuntimeContext::poll_port_messages` reads it through a
`beam_io::Poller`, and `src/eval/mod.rs`'s `receive … after T` polls for up to `T` ms and matches the
produced messages against the receive clauses (falling back to the `after` body). The message's `data`
atom is interned in the runtime's **one** shared table and the term is built on the runtime's
pointer-stable arena, so it is nameable by the printer and matchable by a `receive` clause. A
`binary`-stream read wider than `ERL_ONHEAP_BINARY_LIMIT` (64) bytes is the honest typed *unsupported*
gap (there is no off-heap ref-counted binary build path here yet).

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
The floor is the set of BIFs that are really *runtime* operations — they read or write the runtime's
own state (its process/port tables, code server, atom table, application view, parsed command line)
and are therefore served **before** the `beam_bifs` registry, from `RuntimeContext::try_native`. It is
a **directory module**, split by responsibility:

* `src/native.rs` — the module docs, `try_native` itself (the `(module, function, arity)` dispatch
  table) and the shared helpers (`string_list`/`atom_name`/`atom`/`charlist`/`render`/`write_output`/
  `chars_to_string`), plus the `erlang`-core entries `halt/0,1`, `display/1`, `io:put_chars/1,2`,
  `erlang:apply/2,3`, `erlang:system_info/1` and `os:cmd/1`.
* `src/native/init.rs` — `init:get_argument/1`, `init:get_arguments/0`, `init:get_plain_arguments/0`,
  `init:get_status/0` (→ `{starting, started}`, the terminal boot state).
* `src/native/code.rs` — `code:get_path/0`, `code:all_loaded/0`, `code:which/1`, `code:is_loaded/1`
  (→ `{file, Path}` / atom `preloaded` / atom `false`), `code:is_sticky/1` (boolean) and `code:is_sticky/2`
  (an `error:undef` raise — the release does not have the `/2`), `code:ensure_loaded/1`
  (→ `{module, Mod}` / `{error, nofile}`), `code:lib_dir/0,1`, `code:root_dir/0` (the parent of
  `lib_dir/0`, i.e. the installation root).
* `src/native/process.rs` — the `erlang` process/registry/reference entries: `make_ref/0`, the monitor
  pair `monitor/2` + `demonitor/1,2` (real, over `beam_process`'s monitor machinery and
  [`Runtime::refs_mut`](crate::Runtime::refs_mut)), `ports/0` (the live ports, via
  `PortRegistry::ids`), `processes/0`, `pre_loaded/0` (the oracle's **exact** list *and* order, see
  `src/boot.rs`), `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`.
  `whereis(init)` answers the runtime's own root process — `Runtime::new` registers the name `init` on
  that pid, the same pid `group_leader/0` reports (the oracle answers `<0.0.0>`).
* `src/native/application.rs` — `application:get_env/1,2,3`, `get_all_env/1`, `get_key/2`,
  `loaded_applications/0`, `which_applications/0`, answered from a minimal native app view (the `.app`
  files resolved through the [`CodeServer`](crate::CodeServer) and a recursive-descent parser for the
  `.app` term text). **Honest deviation:** `application:get_all_env(kernel)` reports the `.app` file's
  **declared** env only — the seven pairs `kernel.app` lists (oracle: nine; the two extra are `logger`
  and `connect_all`, injected by kernel's own application start at run time, and are *not* fabricated
  here). The declared pairs agree key-for-key; only their order differs (the oracle's is its
  application controller's internal storage order). `erts` is deliberately not reported loaded.
* `src/native/test_support.rs` — the shared test fixtures for the floor's unit tests.

`beam_bifs` owns the *registry* surface (`bif.tab`: arithmetic, comparison, type tests, term ops,
`lists`/`maps`/`binary`, `io:format`, `ets`, …); a native entry must never duplicate one. A call the
floor does not claim returns `None` and falls through to the registry unchanged (use
`RuntimeContext::try_native`'s dispatch table, not the module list, as the source of truth for what is
native).

### `code:` is the native floor, not a live `code_server`
`code:*`, `init:*` and `application:*` are **preloaded-module surface** (contract A.7): the oracle
serves them from `init.beam`/`code.beam`/`application.beam`, but those modules are written against the
process/message machinery Milestone-1 does not run (for `application` that is the
`application_controller`/`application_master` process pair plus a group leader). `beam_rt` deliberately
ships **no** live `code_server.beam`: OTP's real one needs `erlang:monitor/2`+`demonitor`,
`register`/`whereis`/`process_flag(trap_exit)`, `spawn_link`, a working `send`, a host **timer** for
`after T`, `make_ref/0`, named-ETS resolution, `persistent_term:*`, `filename:*`, `erl_prim_loader:*`
and `erlang:pre_loaded/0` — all outside this crate — and its *state* already lives in `CodeServer`. The
three differential `boot/*` cases that touch `code:*` are **shape-only** (`is_list`/`is_atom`) and are
already satisfied. The wire a future live server must reproduce is the `{code_call, From, Req}` →
`{code_server, Reply}` message pair (the OTP `code_server:call/1` protocol; `beam_engine` currently
rejects `code_server:call/1`'s `recv_marker_*`/`send` by opcode). **`src/native/code.rs`'s `code:*`
behaviour must not change** — it keeps the currently-agreeing cases green.

### `erlang`-namespace fallback and the known-unimplemented list
`RuntimeContext::call_bif` consults the floor, then the `beam_bifs` registry, and only then falls back
to a diagnostic for the `erlang` namespace. That fallback first checks
`error::is_known_unimplemented_bif(&registry, function, arity)`: a **known** OTP `erlang` BIF this
runtime has not implemented raises the capability-gap `unsupported: erlang:X/Y …` (exit 3), never the
plain `'erlang:X/Y is not a BIF'` a typo gets; a genuinely unknown name still falls through to
`error::unknown_bif`. `error::undef()` is the sibling helper for a call that names a function the
release does not have (`code:is_sticky/2`), raising the bare `error:undef` the oracle does.

The **effective** gap set is the curated static list
(`error::KNOWN_UNIMPLEMENTED_ERLANG_BIFS`, `error::listed_as_unimplemented`) **intersected with what the
live registry does not own** — computed at use time, not baked in. So a later `beam_bifs` change that
registers a name the list still carries can never shadow the registry or turn a served BIF into an
`unsupported` gap (e.g. `erlang:phash/2` and `erlang:phash2/1,2` are registered by `beam_bifs` and are
therefore **not** classified here). A registered name left in the static list is dead weight, not a
hazard; the drift guard is
`error::tests::the_effective_gap_set_never_shadows_the_registry`.

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
  `case`/`if`/`begin`/`try`/`catch`/`receive`, `fun`s, and list/map/**binary** comprehensions
  (including a **binary generator**, `<<Seg || <<X>> <= Bin>>`) — evaluated by an `erl_eval`-style
  tree-walk that dispatches arithmetic/comparison/**bit operators** (`band`/`bor`/`bxor`/`bsl`/`bsr`/
  `bnot`, arbitrary-precision) to the **`beam_bifs` `erlang` registry** (semantics are never
  re-implemented — only `and`/`or`/`xor`/`not`/`andalso`/`orelse`/`++`/`--` remain special forms here). Binary segments accumulate a bit buffer, so construction need not be
  byte-aligned (`<<1:1>>`, `<<A:1,B:7>>`, `<<255:12/little>>`); a byte-aligned result is a **binary**,
  a non-byte-aligned one a **bitstring** materialized by `beam_codec::term::make_bitstring` on the
  runtime's long-lived `eval_heap` (`RuntimeContext::eval_heap`). An expression outside the grammar is
  a **capability gap** (see exit code 3 below), not a genuine error.
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
| `port_count` | `Int` — the number of **live** ports (`PortRegistry::len`), the runtime's own port table |
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
* **The scheduler's borrow strategy: move the table (and the runner state) out of the `Runtime`.**
  A pass needs `&mut Runtime` (BIF dispatch, atoms, heaps — what `RuntimeContext` borrows) **and**
  `&mut ProcessTable` (the PCBs the scheduler runs, which `RuntimeContext::for_process` resolves each
  mailbox/send against) at once. The table is a *field* of the runtime, so handing both out of one value
  is impossible — and the crate is `#![deny(unsafe_code)]` and may not alias. `Runtime::run_pass`
  therefore **moves** the `ProcessTable` and the `RunnerState` out (`take_processes`/`put_processes`, a
  `std::mem::replace` with a fresh `ProcessTable::new(SignalAtoms::from_table(runtime.atoms_mut()))`),
  runs the scheduler with those owned values, then puts both back whatever the outcome. The runner also
  takes the pid's `Machine` **out of** its `HashMap` for the duration of the slice (a `HashMap` cannot
  hand out a `&mut` value while its container is still reachable) and puts it back unless the process
  finished. `Runtime::scheduler` persists across passes (it is not moved), so `run_one_slice` followed
  by `run_scheduler` continues where the slice left off.
* **A process's exit reason is not its return value.** BEAM terminates a normally-returning process
  with `normal`, discarding `x(0)`; the runner therefore **records** each process's final `Returned`
  value per pid in `RunnerState::results` (`Runtime::process_result`) and returns
  `RunOutcome::Exit(normal)`. The value lives on the runtime's pointer-stable arena, which M1 never
  resets, so it stays valid for the runtime's lifetime. A process that **raises** records the mapped
  reason term in `RunnerState::exits` (`Runtime::process_exit`) and exits with that very term — the
  failure is observable, never swallowed (`exit_reason` maps a `BeamError` to the error's own atom, then
  a `Raise`'s owned atom, else interns its `Display` text).
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
  the evaluator's `FunCaller` path (`lists:map(fun(X) -> X*X end, [1,2,3])` → `[1,4,9]`, i.e. a loaded
  stdlib module applying an `-eval` closure) and `nif_error`'s live reason term (atom → atom; the tuple
  `{nif_not_loaded,m,f,0}` printed verbatim),
  the native floor (`erlang:halt/0,1` records `Runtime::halt`, `init:get_argument` occurrences/absent),
  `error::unsupported_reason` (`Some` for `bad_eval`/`unsupported_opcode`/`unsupported_bif`, `None` for
  `missing_value`/`unexpected_argument`/`BadArg`/`SystemLimit`) and `error::halt_status`. The binary
  surface is pinned separately: a **binary comprehension**
  (`<< <<(X+1)>> || <<X>> <= <<1,2,3>> >>` → `<<2,3,4>>`), a binary generator driving a list
  comprehension and a **sized pattern** (`[X || <<X:16>> <= <<0,1,0,2>>]` → `[1,2]`), and
  **non-byte-aligned construction** asserted through `beam_codec::term::bitstring_bytes` (no printer
  involved) — `<<1:1>>`, `<<255:12>>`, `<<255:12/little>>`, `<<A:1,B:7>>` and a sized `bits` head
  before a `binary` tail.
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
  extras test (`case`/comprehension/`fun`/`element`/`abs`, goldens verified against system `erl`), a
  probe for the two `semantics/*` **binary-segment** corpus cases (`semantics/binary_comprehension`,
  `semantics/bit_size_of_bitstring`) and an unsupported-classification check.
* **`tests/multimodule.rs` — the multi-module host.** Compiles a fixture with the system `erlc` into a
  scratch dir and runs it through `Runtime::run_function`: `lists:reverse/1` → `[3,2,1]`,
  `maps:get/2` on a map literal → `7`, and the on-demand stdlib module `ordsets`. It **skips loudly**
  (printing the reason) when the system toolchain/stdlib is unavailable, never silently passing.
* **Ports, the port-stream read, nested re-entrancy and bit operators.** `src/port.rs` unit tests (all
  three Name forms, the options, `BadArg` rejection without consuming a port number, `close_port`
  kill/reap, unknown-port `BadArg`, the seeded `port_info` vocabulary, and `ids()` reporting a live
  port across a released *lower* slot); `tests/eval.rs`'s `open_port_spawns_a_real_os_process`
  (the exact `driver/port_backed_by_spawn_driver` probe + argv at the CLI boundary, a real `os_pid`, the
  `port_info` pairs, `port_close/1`) and
  `a_spawn_driver_port_stream_is_read_back_as_a_data_message` (the `driver/spawn_driver_port_stream_read`
  golden `ok <<104,105,10>>`, the `list` default, the bound-port match, the `after` fallback, and
  `an_oversized_port_stream_binary_is_unsupported_not_wrong` — the >64-byte binary read is an honest
  exit-3 capability gap); `tests/port_stream.rs` exercises the **same** read path structurally through
  `Runtime::eval` (`{Port, {data, Data}}` with `Data` a binary / a char list / the `after` fallback);
  `context.rs`'s
  `ets_inside_an_applied_fun_reaches_the_outer_table`, `a_nested_io_format_reaches_the_callers_sink` and
  `a_nested_call_without_a_fun_still_sees_the_outer_tables` (the nested-BIF seams through the real call
  path); `eval/mod.rs`'s bit-operator tests (small-int sanity for each of the six, a `1 bsl 70`
  bignum round-trip, a non-integer operand → `badarith`).
* **The runtime-native floor's unit tests** are colocated in `src/native/*.rs` (fixtures shared from
  `src/native/test_support.rs`): the `erlang`-core entries, the `init:*` command-line surface and boot
  status, every `code:*` entry (including `root_dir`/`is_loaded`/`is_sticky`/`ensure_loaded`), the
  process/reference/monitor/registry entries (`make_ref` uniqueness, the monitor/demonitor matrix,
  `ports`/`processes`/`pre_loaded`/`whereis`/`register`/`unregister`/`module_loaded`), and the
  `application:*` entries plus the `.app`-resource parser (system-installation cases skip **loudly**
  via `system_runtime`). `src/error.rs`'s tests pin `KNOWN_UNIMPLEMENTED_ERLANG_BIFS` (a curated entry
  is a capability gap; **no** name `beam_bifs`' registry owns is on the list) and the `undef` raise.
* **`tests/mailbox.rs` — the nine engine `Mailbox` seam methods (round 1).** Drives `RuntimeContext`'s
  `Mailbox` implementation over a real `beam_message::Queue`: enqueue/len/scan/cursor, the
  `recv_marker_*` table, and that `send_message`'s `from` reflects the running pid — pinned against
  `Runtime.mailboxes` per-pid scan/marker state surviving a blocked `receive`.
* **`tests/scheduler.rs` — the two-process blocking + resume-on-send proof.** Reuses the committed
  engine fixture `../beam_engine/tests/fixtures/recv.beam` (`wait/0` = `receive X -> X end`, `bomb/1` =
  `Pid ! 42, ok`, `sel/0`, `scan/0`) by relative path (`concat!(env!("CARGO_MANIFEST_DIR"), …)`, never
  copied), plus the added `tests/fixtures/sched.beam` (`send2(Pid,N) -> Pid ! 42, Pid ! {bar, N+7}, ok`).
  Tests: spawn a `recv:wait/0` receiver + a `recv:bomb/1` sender seeded with `ProcessId::to_eterm`, run
  the scheduler to completion, assert the receiver's `process_result` == `42` (block + resume end-to-end);
  run the receiver FIRST (assert it is `ProcessState::Waiting` after that slice) then the sender, assert
  the send wakes it; and a cross-process selective-receive case (`sched:send2/2`) where a non-matching
  `42` head is skipped and the matching `{bar,7}` tail delivers `7`. Oracle values verified against the
  system `erl`. **Caveat:** the whole fixtures directory must not be passed to the oracle as a single
  `-pa` — it contains a `lists.beam` that shadows stdlib (the Rust loader resolves system stdlib
  specially, so `Runtime::load_module(FIXTURE)` works).
* **Gate:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt` GREEN;
  the whole-workspace `build`/`test` must stay green. The compatibility gate itself is the sibling
  `tools/differential` harness: build this crate in release
  (`erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt`) then
  run it (`erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p
  differential -- suite`).

## Known Issues / Intentional M1 gaps
* **No interactive shell, no distribution, no node.** `-noshell` is the only mode; `-name`/`-sname`
  are parsed and stored but distribution (A.5) is not started.
* **Preloaded modules are a name-set skeleton.** Boot does **not** load preloaded `.beam`s, and the
  shared atom table is seeded with `CORE_ATOMS` + `INFO_ATOMS` (not the full `atom.names` ordering), so
  positional atom indices for preloaded code are not reproduced. The erts app's `ebin` **is** on the
  code path (`CodeServer::resolve_system_ebins`), so a preloaded module (`init`, `persistent_term`, …)
  *can* be loaded from disk on demand; boot order and the registry are in place.
* **`-eval` results that are not byte-aligned cannot be *printed*.** Construction and `bit_size/1` are
  supported for a non-byte-aligned bitstring (`<<1:1>>`, `<<255:12/little>>`), but the registered
  `beam_bifs` `io:format` `~w`/`~p` printer rejects one with `badarg` — a **`beam_bifs` printer gap
  outside this crate** (the corpus probes read such a value through `bit_size/1`, so they are
  unaffected). The honest segment limits remain: a construction segment **or** a pattern segment wider
  than 64 bits is *unsupported*, and a value that needs a bignum is `system_limit`.
* **The `beam_bifs` `~p` printer rejects a container wider than its ~80-column rendering limit.** The
  term itself is correct, but printing it *inside a container* that does not fit one line fails:
  `io:format("~p~n",[code:is_loaded(lists)])` → `beam_rt: badarg` (exit 1) where the oracle prints
  `{file,"…/lists.beam"}` — another **`beam_bifs` printer gap outside this crate** (verified: a
  `{file,"…"}` tuple renders fine up to ~60 columns and is `badarg` past ~80). It shows up on the
  `code:*`-shaped native answers because they embed an installation path.
* **Scheduler is single-threaded; a few M1 process gaps.** `Runtime::spawn_process` +
  `run_one_slice`/`run_slices`/`run_scheduler` run spawned processes and a blocked receiver **is**
  resumed by a sender (send→wake→resume via `resume_woken`); the run loop is single-threaded (never a
  thread). Remaining gaps, all honest: **no reduction pre-emption** (`budget` is not enforced; `Yield` is
  never returned; a process runs to its next `return`/`wait`); a non-zero `receive ... after T` **suspends
  (`Wait`) instead of timing out** on the *scheduler/runner* path (there is no host timer — a message
  never sent leaves the process `Waiting`; only the `-eval` `receive … after T` path advances, by polling
  ports — see Known Issues below); and on the runner path a **capability gap is uncatchable** — a process raising
  `badmatch`/`case_clause` terminates with the mapped reason (atom or `Display` text) rather than the real
  term. `system_info(process_count)` still reflects the runtime's own process table entry.
* **No live `code_server`.** `code:*` on the receive path is the native floor in `src/native/code.rs` (see the
  `code:` section above), not a running `code_server` process.
* **`-eval`/`-s`/`-run` are documented subsets** (`-s` uses atoms + discards the result, `-run` uses
  strings; `-eval` evaluates the expression subset and prints nothing). An `-eval` expression outside
  the grammar is *unsupported* (exit 3); the M1 expression gaps (`!` send, a bare `receive`) are typed
  errors in `src/eval/mod.rs`. A capability gap (`unsupported_opcode:`/`unsupported_bif`/`bad_eval`)
  is **uncatchable**: it travels as `Flow::Fatal` (like `halt`) so a `try`/`catch` in the probe cannot
  turn it into an apparent exception, and the CLI exits 3. Real `erl`
  start-script processing, `-init_debug` and the rest are absent — and, like any unmodeled switch, are
  recorded as init arguments rather than acted on.
* **Nested-BIF re-entrancy (a `fun` applied inside a BIF) is handled — with one limitation.**
  When a `fun`-taking `lists:*`/`maps:*` BIF calls back through `FunBridge`, the nested `RuntimeContext`
  takes a **fresh pooled heap**, shares the caller's sink through a `SharedSink` clone and reaches the
  runtime's ETS tables: `call_registered` installs the `TableRegistry` **only for a call that cannot
  apply a fun argument** (an argument is not a fun), so the outer call leaves the shared `RefCell` free
  for the nested `ets:*` to borrow. The one limitation: a BIF that needed ETS *and* applied a fun
  argument would not get tables — none exists in M1. Output sharing costs `RuntimeContext` a
  destructor (the `Rc`-backed `SharedSink`), so a caller that keeps a `RuntimeContext` alive cannot
  reuse the `Runtime` binding directly until it goes out of scope; read it through the context instead.
* **Ports: spawned, registered, queryable, and stdout-polled on the `-eval` receive path.** `erlang:open_port/2`
  really spawns the OS process (`{spawn, Cmd}` through `/bin/sh -c`, `{spawn_executable, …}` directly,
  `{fd, N}` as a descriptor port), allocates a `beam_process::PortId` and records the live port (real
  `os_pid`, `connected`, stream type, the child's `stdout` handle); `port_close/1` kills/reaps the child
  and releases the id; `port_info/1,2` answer the seeded item vocabulary; `erlang:ports/0` reports the
  live ports. Deliberate `port_info` deviations from erts-17.1 (never fabricated): `id` is the port
  **term** (erts: a bare integer), `connected` is a **boolean** (erts: the controlling pid) and `stream`
  is reported (erts reports `input`/`output` byte counters); `links`/`monitors` are `[]` (truly empty
  here). **stdout is read back only on the `-eval` `receive … after T` path**
  (`RuntimeContext::poll_port_messages`); the **scheduler/runner** path never polls, so a BEAM process
  blocked in `receive` waiting for a `{Port, {data, …}}` message stays `Waiting` (nothing drives the
  poller from the process mailbox). The read is **one** `Read::read` per readiness (one message per read,
  not a stream coalesced per driver callback); **stdin and stderr are never read** (they are pipes, but
  nothing consumes them); `port_command/2` does not exist (it is on the known-unimplemented list), so
  nothing is ever written to the child's stdin; `connected` flips to `false` only at `port_close/1`,
  not when the child exits by itself; `exit_status`/`eof` are recognized but inert (their only
  observable form is a `{Port, …}` message, which the poller does not synthesize); and a child that
  exits on its own is reaped only by `port_close/1` or at runtime exit. A `binary`-stream read wider
  than 64 bytes is the typed *unsupported* gap (no off-heap ref-counted binary build path yet).
  `os:cmd/1` remains the runtime-native host spawn, unchanged.
* **Heap has no GC** (see Design Decisions) — a runaway allocation returns `SystemLimit`.

## Routing Table
* `src/lib.rs` → crate docs, `#![deny(unsafe_code)]`, module wiring, root re-exports, `prelude`
* `src/error.rs` → `pub(crate)` `BeamError` constructors (missing value, unexpected argument, bad spec, arity mismatch, bad run argument, unknown module/function/info key/bif, unsupported, bad eval, `undef`) + the halt helpers (`HALT_PREFIX`, `halt`/`halt_status`) + `unsupported_reason` (the single capability-gap classifier) + the curated `KNOWN_UNIMPLEMENTED_ERLANG_BIFS` candidate list with `listed_as_unimplemented` and `is_known_unimplemented_bif(registry, …)` — the **effective** gap set, i.e. the list **intersected with what the live `BifRegistry` does not own**, so the static list and the registry can never drift — used by the `erlang`-namespace fallback — no crate-local error enum
* `src/config.rs` → `RuntimeConfig`, `Action`, `ArgTerm`, `InitArgs`, `EmulatorConfig` alias, defaults/setters
* `src/cli.rs` → `USAGE`, `parse` (the A.6 parser + the `run` argument grammar), `run` (the CLI driver + the four exit-code constants `EXIT_OK`/`EXIT_RUNTIME`/`EXIT_USAGE`/`EXIT_UNSUPPORTED`)
* `src/codeserver.rs` → `CodeServer`: the **one** shared atom table (held as an `Rc<AtomTable>`, copy-on-write on intern — see Design Decisions) + module cache + code path + `load`/`load_path`/`load_bytes`/`add_system_paths`/`find_file`/`loaded_path`/`lib_root`/`app_dir`/`atoms_handle` (the load-on-demand host); `resolve_system_ebins` puts the stdlib, kernel **and erts** `ebin` dirs on the path (the erts app is where the preloaded modules' `.beam`s — `init`, `persistent_term`, … — live), all resolved at run time (never baked in)
* `src/boot.rs` → `PreloadedModules` skeleton (A.7): `PRELOADED_MODULE_NAMES` is the oracle's **exact** `erlang:pre_loaded/0` list *and* order (`persistent_term` first … `erts_code_purger` last, verified against OTP 29/erts-17.1), which `pre_loaded/0` and `code:all_loaded/0` reproduce verbatim + the core-atom seed (also interns `info::INFO_ATOMS` and `port::PORT_INFO_ATOMS`) + the boot banner
* `src/port.rs` → the A.4 `PortHook` provider: `PortRegistry` (a `beam_process::PortTable` + an id → `LivePort` map + a per-registry scratch `Heap`; `contains`/`len`/`is_empty`/`ids`), `LivePort` (which keeps the child's `stdout` handle so the stream can be read), `RuntimePorts` (the `beam_bifs::PortHook`: Name/Options parsing + validation → `BeamError::BadArg`, the spawn driver via `std::process::Command`, kill/reap on close, real `port_info` pairs), `RuntimeContext::poll_port_messages` (the `beam_io::Poller`-driven read that produces `{Port, {data, Data}}` messages), `PortName`/`StreamType`/`PORT_INFO_ATOMS`
* `src/info.rs` → `SUPPORTED_KEYS`, `INFO_ATOMS`, `InfoValue`, `system_info_value` (A.8 keys), `InfoValue::to_eterm`
* `src/context.rs` → `RuntimeContext`: the concrete `ExecutionContext` glue (native floor → `beam_bifs` registry → load-on-demand export dispatch; `apply_fun` routes an `-eval` fun (`EVAL_FUN_UNIQ`) to `eval::apply_eval_closure`; heap check); `eval_heap` exposes the runtime's long-lived `-eval` heap a non-byte-aligned bitstring is materialized on; `PendingInterner` interns a BIF-built new atom into the one shared table (predicted index, committed after the call); `SharedSink` — the cloneable `Rc<RefCell<..>>`-backed output handle a nested context shares; `reentrancy_capable` (installs the `TableRegistry` only for a call that cannot apply a fun argument, so `ets:*` inside a fun reaches the outer tables); `FunBridge` — the concrete `beam_bifs::FunCaller` the `fun`-taking `lists:*`/`maps:*` BIFs call back through; `call_registered` installs exactly these three `BifContext` seams (`AtomInterner`/`FunCaller`/`PortHook`) and **no** `SendHook`, so the `beam_bifs`-registered `erlang:send/2` / `erlang:'!'/2` raise `unsupported_bif` (message delivery is owned by a future `beam_process`/`beam_message` wiring); its `erlang`-namespace fallback consults `error::is_known_unimplemented_bif` **after** the registry dispatch, so a known-but-unimplemented OTP `erlang` BIF is `unsupported:` (exit 3) rather than a plain `is not a BIF` diagnostic
* `src/native.rs` → the runtime-native BIF floor: the module docs, `try_native` (the `(module, function, arity)` dispatch consulted **before** the registry) and the shared helpers; the `erlang`-core entries `halt/0,1`, `display/1`, `io:put_chars/1,2`, `erlang:apply/2,3`, `erlang:system_info/1`, `os:cmd/1`; declares the submodules below
* `src/native/init.rs` → the `init:*` entries (`get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`, `get_status/0`)
* `src/native/code.rs` → the `code:*` entries (`get_path/0`, `all_loaded/0`, `which/1`, `is_loaded/1`, `is_sticky/1,2`, `ensure_loaded/1`, `lib_dir/0,1`, `root_dir/0`) over the `CodeServer` state + the preloaded registry
* `src/native/process.rs` → the `erlang` process/registry/reference entries (`make_ref/0`, `monitor/2`, `demonitor/1,2`, `ports/0`, `processes/0`, `pre_loaded/0`, `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`) over `beam_process`'s tables + the runtime reference id space
* `src/native/application.rs` → the `application:*` entries (`get_env/1,2,3`, `get_all_env/1`, `get_key/2`, `loaded_applications/0`, `which_applications/0`) over a minimal native app view (the `.app` files resolved through the `CodeServer` + a recursive-descent `.app` term-text parser). Its production half is ~700 lines; the file legitimately reads past the ~1000-line baseline because of the comprehensive colocated `#[cfg(test)] mod tests` (stated in the file header) — it is **not** restructured
* `src/native/test_support.rs` → the floor's shared test fixtures (`runtime`/`system_runtime`/`call`/`atom`/`chars`/`string_of`)
* `src/eval/mod.rs` → the `-eval` evaluator: an `erl_eval`-style tree-walk over the AST with its own `Flow` control signal (live raise reasons; `halt` **and** a capability gap are uncatchable), dispatching calls through `RuntimeContext`; binary-comprehension evaluation (`Qual::BinaryGenerator`) and the bit-buffer construction (`BitBuf`/`materialize_bits`, byte-aligned → binary, else a bitstring on `eval_heap`); `apply_eval_closure` runs a stored closure when a loaded module applies an `-eval` fun
* `src/eval/ast.rs` → the `-eval` AST (`Expr`/`Clause`/`Try`/`Seg`/`Qual`/`MapField`/`BinOp`/`UnOp`)
* `src/eval/parse.rs` → the `-eval` lexer + recursive-descent/Pratt parser (Erlang precedence; the `<=` binary-generator qualifier; a segment's `:Size` parsed in segment context so a following `/Type` is a specifier, not a division; malformed input → `error::bad_eval`, exit 3)
* `src/format.rs` → `term_to_string`/`term_to_string_w` — drives the registered `io:format/2` `~p`/`~w` printer
* `src/mailbox.rs` → `RuntimeContext`'s nine `beam_engine::Mailbox` seam methods over the running process's real `beam_message::Queue`, with per-pid scan/marker state in `Runtime.mailboxes: HashMap<ProcessId, MailboxState>` (the receive cursor + the `recv_marker_*` table); `MailboxState` + `Runtime::mailboxes` are the durable position a blocked `receive` resumes against
* `src/runner.rs` → the `beam_process::ProcessRunner`: `ProcessEntry`, `ProcessEngine`, `RunnerState` (`engines`/`results`/`exits`), `RuntimeRunner` (implements `ProcessRunner::run` over `RuntimeContext::for_process`; first run `run_export_outcome`, resume `resume_outcome`; maps `Returned`→record+`Exit(normal)`, `Wait`→park+`ProcessState::Waiting`+`RunOutcome::Wait`, `Err`→`exit_reason` term+`Exit`); `exit_reason` maps a `BeamError` to an atom reason (never panics); module docs carry the borrow strategy + the "exit reason is not the return value" rule
* `src/runtime.rs` → `Runtime`: boot-wiring, code server, module cache, `run_function`/`run_module`/`run_named` (the slice), the **scheduler API** (`spawn_process`/`enqueue_process`/`run_one_slice`/`run_slices`/`run_scheduler`/`process_result`/`process_exit`/`run_queue_len` + the private `run_pass`/`take_processes`/`put_processes` move-out dance), `materialize_args`, `system_info`, `eval`, `format_term`, `eval_closures` (the `-eval` fun store shared with `RuntimeContext::apply_fun`) and `eval_heap` (the long-lived heap a non-byte-aligned `-eval` bitstring is built on — see `RuntimeContext::eval_heap`); `refs_mut` (the one `beam_process::RefAllocator` shared by `make_ref/0` and monitor/demonitor); fields `processes` (`pub(crate)`), `mailboxes`, `scheduler`, `runner_state`, `refs`, `references` (the reference→monitor map `demonitor/1` reads)
* `src/main.rs` → the thin executable shim over `cli::run`
* `tests/slice.rs` → the end-to-end library vertical slice over the real `.beam` fixture
* `tests/cli.rs` → the executable end-to-end (spawns `CARGO_BIN_EXE_beam_rt`)
* `tests/eval.rs` → the `-eval` end-to-end: all 17 differential `Eval` probes pinned to exact stdout + exit, plus the `driver/port_backed_by_spawn_driver` port probe (a real OS process, the `port_info` pairs, `port_close/1`), the `driver/spawn_driver_port_stream_read` port-stream probe (the `ok <<104,105,10>>` golden, the `list` default, the bound-port match, the `after` fallback, the >64-byte binary read as an exit-3 capability gap), the two `semantics/*` binary-segment probes, control-flow extras and the unsupported-classification check
* `tests/port_stream.rs` → the port-stream read at the **library** boundary (`Runtime::eval`), asserting the `{Port, {data, Data}}` term structurally (binary payload, char-list payload, and the `after` fallback) rather than only through the printer
* `tests/multimodule.rs` → the multi-module host test (compiles a fixture with the system `erlc`, calls `lists:reverse/1` + `maps:get/2` + `io:format/2` + the on-demand stdlib `ordsets`; skips loudly without a system stdlib)
* `../beam_engine/src/context.rs` → the `ExecutionContext` seam this crate implements (read it; do not change)
* `../beam_bifs/` → the `BifRegistry`/`BifContext`/sinks this crate drives (read-only)
* `../beam_loader/tests/fixtures/slice.beam` → the shared oracle fixture reused by the slice tests (read-only)
* `../../erts/emulator/beam/erl_bif_info.c`, `../../erts/etc/common/erlexec.c`, `../../erts/emulator/sys/unix/sys.c`, `../../erts/emulator/beam/erl_init.c` → A.6/A.7/A.8 ground truth (read-only)

## Handoffs (owned by OTHER nodes)
`differential suite` is at **`summary: total=285 agree=281 mismatch=0 unsupported=4 skipped=0`**.
Remaining asks owned elsewhere:
* **The 4 `unsupported` are the `wire/*` ETF-encode cases**, owned by `beam_codec`/`beam_dist`.
* **`beam_engine` — name the rejected opcode.** Kernel `code.beam`'s `code_server:call/1` executes `recv_marker_reserve/1` (also `recv_marker_bind`, `send`, `loop_rec`, `remove_message`), which no family claims; it falls to the generic `Err(error::unsupported("widened opcode not yet executed"))` at `beam_engine/src/families/mod.rs:93`, which does **not** name the instruction (`Instr::Generic` already does at `interpreter.rs:366`). The three `boot/*` cases that used to surface this now agree via the native `code:*` floor, so this is a diagnostic-quality ask, not a gate blocker.

## See Also
* `../CONTEXT.md` → crate-layer contracts, the seam types, the wave-6 position, the M1 scope table.
* `../../CONTEXT.md` §1 A.6/A.7/A.8 → CLI/configuration, preloaded/boot, introspection contracts; §3 error strategy; §7 staging.
* `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.6–A.8 → the CLI/config, OTP-app and introspection surfaces.
* `../beam_engine/CONTEXT.md` (the seam), `../beam_bifs/CONTEXT.md` (the BIF registry), `../beam_loader/CONTEXT.md` (the IR + fixture).
