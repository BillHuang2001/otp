//! The runtime façade: boot, the code server and the **vertical slice**.
//!
//! [`Runtime`] is the whole picture in one object. It owns
//!
//! * the parsed [`RuntimeConfig`] and its init arguments (contract A.6);
//! * the BIF [`BifRegistry`] (`beam_bifs`'s `bif.tab` surface) and a
//!   [`TableRegistry`] (`ets:*`);
//! * the [`CodeServer`] — the **one shared** [`AtomTable`](beam_term::AtomTable),
//!   the module cache and the ordered code path. Every module the runtime loads is
//!   interned into that single table, so atoms built by a BIF are the indices the
//!   engine compares against for **every** loaded module;
//! * the [`ProcessTable`] and the runtime's own [`ProcessId`];
//! * the preloaded-module registry skeleton (contract A.7);
//! * the process [`Heap`] (owned, so a term returned by `run_*` stays valid), a
//!   separate scratch `info_heap` for `system_info`/`format_term`, and the
//!   pointer-stable [`MinimalContext`] arena that serves the engine's
//!   `alloc_words`/`ensure_heap` seam (Milestone-1 has no GC).
//!
//! `Runtime::run_function`/`run_module` are the evidence for this crate: they load a
//! real `.beam` and execute it through the real [`RuntimeContext`] and the real BIF
//! registry — the same path the `beam_rt` executable takes. The dispatch that makes
//! this a *multi-module* host (native floor → `beam_bifs` registry → load-on-demand
//! external call) lives in [`RuntimeContext::call_bif`](crate::context).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{BifRegistry, OutputSink, TableRegistry};
use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext, MinimalContext};
use beam_loader::Module;
use beam_process::{
    MonitorRef, ProcessId, ProcessState, ProcessTable, RefAllocator, Scheduler, SignalAtoms,
};
use beam_term::{atom_val, is_atom, make_atom, make_small, AtomTable, Eterm};

use crate::boot::{self, PreloadedModules};
use crate::codeserver::CodeServer;
use crate::config::{ArgTerm, InitArgs, RuntimeConfig};
use crate::context::RuntimeContext;
use crate::error;
use crate::eval::FunBody;
use crate::format;
use crate::info::{self, InfoValue};
use crate::mailbox::MailboxState;
use crate::port::PortRegistry;
use crate::runner::{ProcessEntry, RunnerState, RuntimeRunner};
/// The size in term words of the scratch heap `system_info`/`format_term` build on.
///
/// It is `reset()` at the start of every such call, so it only has to hold one
/// materialized value.
const INFO_HEAP_WORDS: usize = 4_096;

/// The smallest process heap a runtime accepts (`heap_words` is floored at this).
const MIN_HEAP_WORDS: usize = 1_024;

/// The Rust ERTS runtime.
pub struct Runtime {
    /// The configuration this runtime was created with.
    config: RuntimeConfig,
    /// The BIF registry every `call_bif` is dispatched through.
    ///
    /// Shared through an [`Rc`] so a `BifContext` can be dispatched against a cheap clone
    /// that does not borrow the runtime (see [`crate::context::RuntimeContext::call_registered`]).
    pub(crate) registry: Rc<BifRegistry>,
    /// The code server: the one shared atom table, the module cache, the code path.
    pub(crate) code_server: CodeServer,
    /// The process table (Milestone-1: the runtime's own single process).
    ///
    /// `pub(crate)` so [`RuntimeContext`](crate::context::RuntimeContext) can resolve a
    /// process's mailbox against it (the receive seam) without a runtime accessor.
    pub(crate) processes: ProcessTable,
    /// The per-process **mailbox scan state** — the receive cursor and the
    /// `recv_marker_*` table — keyed by pid.
    ///
    /// It lives here, not in a context, because a `receive` that suspends on an empty
    /// mailbox is resumed against exactly this position once a message arrives (see
    /// `crate::mailbox`). A pid with no entry has never scanned, which is the default
    /// state (cursor at the first message, no markers).
    pub(crate) mailboxes: HashMap<ProcessId, MailboxState>,
    /// The preloaded-module registry skeleton (contract A.7).
    preloaded: PreloadedModules,
    /// The heaps BIF calls allocate on, used as a stack: a call takes one out, runs, and
    /// puts it back (see [`Runtime::take_bif_heap`]). A `beam_alloc::Heap` never moves its
    /// terms and is never reset in Milestone-1 (there is no GC), so a heap must outlive
    /// every term built on it — reusing one heap for sequential calls keeps that free,
    /// while a nested call (inside a fun a BIF applied) gets its own heap.
    pub(crate) bif_heaps: Vec<Heap>,
    /// The size in term words of a freshly created BIF heap.
    pub(crate) bif_heap_words: usize,
    /// The single-threaded scheduler: the run queue of the processes
    /// [`Runtime::spawn_process`] created, plus its thread-progress counter.
    ///
    /// It persists across calls, so [`Runtime::run_one_slice`] followed by
    /// [`Runtime::run_scheduler`] continues exactly where the slice left off.
    scheduler: Scheduler,
    /// The per-pid engine state (the machine, its entry point and the address a blocked
    /// `receive` resumes at) and the recorded outcomes of every spawned process.
    ///
    /// It lives here rather than in a pass-local value because it must outlive a single
    /// scheduler pass — a process blocked in a `receive` is resumed by a *later* pass —
    /// and it is **moved out** for the duration of a pass so the runner can hold it at the
    /// same time as the process table (see [`crate::runner`]).
    runner_state: RunnerState,
    /// The pointer-stable arena serving the engine's `alloc_words` seam.
    pub(crate) arena: MinimalContext,
    /// The scratch heap for `system_info`/`format_term`, reset per call.
    pub(crate) info_heap: Heap,
    /// The long-lived heap for terms the `-eval` evaluator must build on a
    /// [`beam_alloc::Heap`] rather than the engine arena — today a non-byte-aligned
    /// bitstring, which [`beam_codec::term::make_bitstring`] materializes.
    ///
    /// Like [`Runtime::bif_heaps`] it never moves its terms and Milestone-1 never
    /// resets it (there is no GC), so a term built on it stays valid for the runtime's
    /// lifetime.
    pub(crate) eval_heap: Heap,
    /// The `ets:*` tables.
    ///
    /// Shared through an [`Rc`]`<`[`RefCell`]`>` so a BIF call can hold the registry for
    /// the duration of its call without borrowing the runtime; a **nested** call (a fun
    /// applied by a BIF) finds it already borrowed and runs without ETS — the `ets:*` BIFs
    /// then raise `unsupported_bif`, never a wrong result.
    pub(crate) tables: Rc<RefCell<TableRegistry>>,
    /// The port table and the live ports behind `erlang:open_port/2`.
    ///
    /// Shared through an [`Rc`]`<`[`RefCell`]`>` for the same reason as
    /// [`Runtime::tables`]: a BIF call holds it for the duration of its call without
    /// borrowing the runtime, so the fun-caller bridge can borrow the runtime at the same
    /// time (see [`crate::context::RuntimeContext::call_registered`]). A **nested** borrow
    /// (a port operation re-entered from inside a port operation) is reported as a typed
    /// capability gap rather than panicking.
    pub(crate) ports: Rc<RefCell<PortRegistry>>,    /// The pid code runs as (`erlang:self/0`).
    pub(crate) self_pid: ProcessId,
    /// Whether [`Runtime::boot`] has run.
    booted: bool,
    /// The exit status recorded by a native `erlang:halt/1` (see [`crate::native`]).
    pub(crate) halt: Option<i32>,
    /// The reference id space every `erlang:make_ref/0` and `erlang:monitor/2` draws from.
    ///
    /// One space for both, so a bare reference and a monitor reference can never collide
    /// (see `crate::native::process`).
    pub(crate) refs: RefAllocator,
    /// Every reference term the runtime minted, keyed by the term itself.
    ///
    /// A reference term is the address of a freshly allocated 4-word thing, so it is a
    /// unique key; the entry is `Some(MonitorRef)` for a monitor reference and `None` for a
    /// bare `make_ref/0`. This is how `erlang:demonitor/1` recovers the monitor a reference
    /// stands for without reading the thing's payload — which would need `unsafe`, and this
    /// crate is `#![deny(unsafe_code)]`.
    pub(crate) references: HashMap<Eterm, Option<MonitorRef>>,
    /// Closures built by the `-eval` evaluator, indexed by the fun term's `index`.
    ///
    /// An `-eval` `fun` may be applied *inside a loaded module* (e.g.
    /// `lists:map(fun(X) -> X*X end, …)` calls back into the closure), so the closure
    /// bodies must outlive the one evaluation and be reachable from
    /// [`RuntimeContext::apply_fun`](crate::context::RuntimeContext). The evaluator
    /// stamps its funs with the sentinel `uniq` [`crate::eval::EVAL_FUN_UNIQ`] and pushes
    /// the body here; the store is append-only, so an index is stable for the runtime's
    /// lifetime.
    pub(crate) eval_closures: Vec<Rc<FunBody>>,
}

impl Runtime {
    /// Creates a runtime for `config`.
    ///
    /// The registries, the shared atom table, the process table and the heaps are set up
    /// immediately; the A.7 **boot step** runs on demand (or explicitly through
    /// [`Runtime::boot`]). The code path is seeded from `config.code_paths`; the system
    /// library directories are added lazily by [`CodeServer`] when a module is first
    /// needed, so no external process is spawned until then.
    ///
    /// # Errors
    ///
    /// Propagates heap/process allocation failures and an out-of-range `+P`-style
    /// limit.
    pub fn new(config: RuntimeConfig) -> Result<Runtime> {
        let mut code_server = CodeServer::new();
        boot::seed_core_atoms(code_server.atoms_mut());
        for dir in &config.code_paths {
            code_server.add_path(dir.clone());
        }
        let signals = SignalAtoms::from_table(code_server.atoms_mut());
        let mut processes = ProcessTable::new(signals);
        let self_pid = processes.spawn_default()?;
        // The runtime's own root process stands in for OTP's `init`: it is the process the
        // runtime boots and runs the start action as, and the one `erlang:group_leader/0`
        // already reports (`src/native/process.rs`). Registering the name is what makes
        // `erlang:whereis(init)` answer that live pid — the oracle answers `<0.0.0>` — and
        // makes `erlang:registered/0`/`erlang:whereis/1` honest about the runtime's own
        // name registry rather than reporting an empty one. `init` is in the core-atom seed
        // interned two lines up, so the lookup cannot miss.
        let init_name = code_server
            .atoms()
            .lookup("init")
            .ok_or_else(|| error::message("boot: the `init` atom is not seeded"))?;
        processes
            .register(init_name, self_pid)
            .map_err(|_| error::message("boot: the `init` name is already registered"))?;
        // A zero-word heap can hold nothing, so the configured size is floored: the
        // runtime must always be able to run the smallest program.
        let heap_words = config.heap_words.max(MIN_HEAP_WORDS);
        Ok(Runtime {
            config,
            registry: Rc::new(BifRegistry::with_core_bifs()),
            code_server,
            processes,
            mailboxes: HashMap::new(),
            preloaded: PreloadedModules::standard(),
            bif_heaps: vec![Heap::new(heap_words)?],
            bif_heap_words: heap_words,
            scheduler: Scheduler::new(),
            runner_state: RunnerState::default(),
            arena: MinimalContext::new(),
            info_heap: Heap::new(INFO_HEAP_WORDS)?,
            eval_heap: Heap::new(heap_words)?,
            tables: Rc::new(RefCell::new(TableRegistry::new())),
            ports: Rc::new(RefCell::new(PortRegistry::new()?)),            self_pid,
            booted: false,
            halt: None,
            refs: RefAllocator::new(),
            references: HashMap::new(),
            eval_closures: Vec::new(),
        })
    }

    /// The default configuration: [`RuntimeConfig::default`], booted on demand.
    ///
    /// # Errors
    ///
    /// As [`Runtime::new`].
    pub fn with_defaults() -> Result<Runtime> {
        Runtime::new(RuntimeConfig::default())
    }

    /// Runs the **boot sequence** (contract A.7). Idempotent.
    ///
    /// Milestone-1 boots the *registry* of preloaded modules and the atom seed built
    /// in [`Runtime::new`]; it does **not** load the preloaded `.beam`s (see the crate
    /// `CONTEXT.md`, Known Issues). A boot with an empty registry is refused rather
    /// than silently producing a runtime without its foundation.
    ///
    /// Nothing is printed: the CLI must be silent on stderr for `-eval`/`run`/`-s`, so
    /// boot emits no banner (the version banner is [`Runtime`]'s `-version` action).
    ///
    /// # Errors
    ///
    /// [`BeamError::Raise`](beam_common::BeamError::Raise) when the preloaded-module
    /// registry is empty.
    pub fn boot(&mut self) -> Result<()> {
        if self.booted {
            return Ok(());
        }
        if self.preloaded.is_empty() {
            return Err(error::unsupported("empty preloaded-module registry"));
        }
        self.booted = true;
        Ok(())
    }

    /// Whether the boot sequence has run.
    pub fn is_booted(&self) -> bool {
        self.booted
    }

    /// The configuration this runtime was created with.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// The parsed init arguments (the source of `init:get_argument/1`).
    pub fn init_args(&self) -> &InitArgs {
        &self.config.init_args
    }

    /// The preloaded-module registry skeleton (contract A.7).
    pub fn preloaded(&self) -> &PreloadedModules {
        &self.preloaded
    }

    /// The code server (the shared atom table, the module cache, the code path).
    pub fn code_server(&self) -> &CodeServer {
        &self.code_server
    }

    /// The shared atom table, mutably (a caller interning an atom before a run).
    pub fn code_server_atoms_mut(&mut self) -> &mut AtomTable {
        self.code_server.atoms_mut()
    }

    /// The shared atom table.
    pub fn atoms(&self) -> &AtomTable {
        self.code_server.atoms()
    }

    /// Interns `name` in the shared atom table and returns it as an atom term.
    pub fn intern(&mut self, name: &str) -> Eterm {
        make_atom(self.code_server.atoms_mut().intern(name))
    }

    /// The name of the atom `term`, if `term` is an atom.
    pub fn atom_name_of(&self, term: Eterm) -> Option<&str> {
        if !is_atom(term) {
            return None;
        }
        self.code_server.atoms().name(atom_val(term))
    }

    /// The exit status recorded by a native `erlang:halt/1`, if one ran.
    pub fn halt_status(&self) -> Option<i32> {
        self.halt
    }

    /// Heap words still available to the running code (unbounded in Milestone-1).
    pub fn arena_available(&self) -> usize {
        self.arena.heap_available()
    }

    /// The runtime's one reference id space (the source of `erlang:make_ref/0` and every
    /// `erlang:monitor/2` reference — see [`Runtime::references`]).
    pub fn refs_mut(&mut self) -> &mut RefAllocator {
        &mut self.refs
    }

    /// Takes a heap for one BIF call out of the pool.
    ///
    /// The pool is a stack: a call takes a heap, runs, and puts it back
    /// ([`Runtime::put_bif_heap`]). Reusing the same heap for sequential calls costs
    /// nothing and keeps every term built on it valid (a `beam_alloc::Heap` never moves
    /// its terms and Milestone-1 never resets one — there is no GC). A **nested** call (a
    /// fun a BIF applied) finds the pool empty and gets a fresh heap, so the outer call's
    /// heap is never aliased.
    ///
    /// # Errors
    ///
    /// Propagates the fresh heap's allocation failure.
    pub(crate) fn take_bif_heap(&mut self) -> Result<Heap> {
        match self.bif_heaps.pop() {
            Some(heap) => Ok(heap),
            None => Heap::new(self.bif_heap_words),
        }
    }

    /// Returns a heap taken by [`Runtime::take_bif_heap`] to the pool.
    pub(crate) fn put_bif_heap(&mut self, heap: Heap) {
        self.bif_heaps.push(heap);
    }
    /// Allocates `n` raw heap words from the pointer-stable arena.
    ///
    /// # Errors
    ///
    /// Propagates the arena's allocation failure.
    pub fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    /// Copies a scheduled process's return value onto the runtime's long-lived,
    /// never-reset `-eval` arena and returns the copy.
    ///
    /// A process's code runs on a heap that dies with it, and a value taken out of the
    /// mailbox lives in exactly that heap — so an `x(0)` that is a container or a binary
    /// would dangle the moment the process terminated. The runner records every return
    /// value through here ([`crate::runner`]) so
    /// [`Runtime::process_result`](Runtime::process_result) really does hand back a term
    /// valid for the runtime's lifetime — the property the scheduler tests rely on (see
    /// `tests/port_wake.rs`, where the receiver returns the port's stream binary).
    ///
    /// The destination is the same pointer-stable arena the `-eval` evaluator's
    /// non-byte-aligned bitstrings use ([`Runtime::eval_heap`]); nothing resets it in
    /// Milestone 1. An immediate is returned unchanged (the copier shares immediates).
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the value does not fit. That is an honest
    /// error, never a dangling term.
    pub(crate) fn stabilize_term(&mut self, term: Eterm) -> Result<Eterm> {
        beam_message::copy_struct_simple(term, &mut self.eval_heap)
    }
    /// Ensures `heap_need` words are available in the arena.
    ///
    /// # Errors
    ///
    /// Propagates the arena's failure (never in Milestone-1; see the module docs).
    pub fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    /// The process table (Milestone-1: one process — the runtime itself).
    pub fn processes(&self) -> &ProcessTable {
        &self.processes
    }

    /// The pid the runtime runs as.
    pub fn self_pid(&self) -> ProcessId {
        self.self_pid
    }

    /// Loads `path` into the code server and returns the cached module.
    ///
    /// A module loaded twice replaces the cached one (mirroring a code reload).
    ///
    /// # Errors
    ///
    /// Propagates the loader's error (a malformed container, an unsupported chunk, …)
    /// or returns a diagnostic when the module has no resolvable name.
    pub fn load_module(&mut self, path: impl AsRef<Path>) -> Result<&Module> {
        let module = self.code_server.load_path(path)?;
        let name = module
            .atom_name(module.name)
            .ok_or_else(|| error::unknown_module("<unnamed .beam>"))?
            .to_string();
        self.code_server
            .module(&name)
            .ok_or_else(|| error::unknown_module(&name))
    }

    /// A cached module by name, if it has been loaded.
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.code_server.module(name)
    }

    /// The names of the cached modules, sorted.
    pub fn loaded_modules(&self) -> Vec<String> {
        self.code_server.loaded()
    }

    /// Finds `<module>.beam` in the configured code path (then the system paths).
    pub fn find_module_file(&mut self, module: &str) -> Option<PathBuf> {
        if let Some(found) = self.code_server.find_file(module) {
            return Some(found);
        }
        // The code server adds the system library paths lazily; a failure here is
        // reported honestly by `CodeServer::load`, so an exploratory lookup is `None`.
        if self.code_server.add_system_paths().is_ok() {
            self.code_server.find_file(module)
        } else {
            None
        }
    }

    /// **The slice.** Loads `path`, caches the module, runs `function/arity` with
    /// `args` and returns `x(0)`.
    ///
    /// `io:format/2` output of the executed code goes to the process's stdout; the
    /// *result* is not printed (the caller decides — see [`crate::cli`]).
    ///
    /// # Errors
    ///
    /// * the loader's error for `path`;
    /// * a diagnostic when the module has no name or does not export `function/arity`;
    /// * any error the running code raises (BIF failures included).
    pub fn run_function(
        &mut self,
        path: impl AsRef<Path>,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.boot_if_needed()?;
        let module = self.code_server.load_path(path)?;
        let mut context = RuntimeContext::new(self);
        context.run_export(&module, function, arity, args)
    }

    /// The general primitive: run `function/arity` against a **caller-owned** module
    /// with an explicit output sink.
    ///
    /// # Errors
    ///
    /// A diagnostic when `function/arity` is not in the module's atom table or is not
    /// exported, plus any error the running code raises.
    pub fn run_module(
        &mut self,
        module: &Module,
        function: &str,
        arity: u32,
        args: &[Eterm],
        out: &mut dyn OutputSink,
    ) -> Result<Eterm> {
        self.boot_if_needed()?;
        let mut context = RuntimeContext::with_output(self, out);
        context.run_export(module, function, arity, args)
    }

    /// Loads the module `name` from the code path and runs its exported
    /// `function/arity`, discarding nothing (the result is returned).
    ///
    /// This is what `-s`/`-run` use: the module is resolved through the code path
    /// (including the system library directories), not from a file argument.
    ///
    /// # Errors
    ///
    /// The code server's not-found diagnostic, plus any error the running code raises.
    pub fn run_named(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.boot_if_needed()?;
        // `find_module_file` also seeds the system library directories (stdlib, kernel)
        // on first use, so `-s lists …` reaches the real stdlib. A miss here is not
        // fatal: `CodeServer::load` below reports it, naming every directory searched.
        let _ = self.find_module_file(module);
        let loaded = self.code_server.load(module)?;
        let mut context = RuntimeContext::new(self);
        context.run_export(&loaded, function, arity, args)
    }

    // --- spawning and scheduling (the process runner's public API) ------------

    /// Spawns a process whose entry point is `module:function/arity(args)` and makes it
    /// **runnable** (it is appended to the scheduler's run queue).
    ///
    /// The module is resolved through the code path exactly as an external call is (`-pa`/
    /// `-pz` directories first, then the system library `ebin`s), so it is loaded on
    /// demand and cached in the one shared atom table. `args` must already be valid terms:
    /// the runtime never moves them (Milestone 1 has no GC), and they are seeded into the
    /// process's `x(0..arity)` when it first runs.
    ///
    /// The process does **not** run until [`Runtime::run_slices`]/
    /// [`Runtime::run_scheduler`] is called. Its `x(0)` is recorded and readable through
    /// [`Runtime::process_result`] once it has run to its `return` (a normally-returning
    /// process's *exit reason* is `normal`, not that value — see [`crate::runner`]).
    ///
    /// # Errors
    ///
    /// * the code server's not-found/malformed diagnostic for `module`;
    /// * a diagnostic when `function/arity` is not exported by it;
    /// * the process table's allocation failure.
    pub fn spawn_process(
        &mut self,
        module: &str,
        function: &str,
        args: Vec<Eterm>,
    ) -> Result<ProcessId> {
        self.boot_if_needed()?;
        // A module the code server already holds (loaded explicitly by
        // [`Runtime::load_module`], say) needs no code-path search — which also avoids
        // resolving the system library directories. Otherwise a miss here is not fatal:
        // `CodeServer::load` reports it, naming every directory searched.
        if self.code_server.module(module).is_none() {
            let _ = self.find_module_file(module);
        }
        let loaded = self.code_server.load(module)?;
        let name = loaded.atom_name(loaded.name).unwrap_or("<unnamed>").to_string();
        let arity = args.len() as u32;
        let function_atom = loaded
            .atom_table
            .lookup(function)
            .ok_or_else(|| error::unknown_function(&name, function, arity))?;
        if loaded.export(function_atom, arity).is_none() {
            return Err(error::unknown_function(&name, function, arity));
        }
        let pid = self.processes.spawn_default()?;
        self.runner_state.spawn(
            pid,
            ProcessEntry::Export {
                module: loaded,
                function: function_atom,
                arity,
            },
            args,
        );
        self.enqueue_process(pid)?;
        Ok(pid)
    }

    /// Spawns a process whose entry point is the **fun** `fun`, applied to `args`.
    ///
    /// This is the process half of `erlang:spawn/1` (and the fun head of
    /// `spawn_link/1`/`spawn_opt/2`): the child is created in the runtime's own process
    /// table, its engine entry registered as a
    /// [`ProcessEntry::Fun`](crate::runner::ProcessEntry::Fun), and it is appended to the
    /// scheduler's run queue — so it runs on the next scheduler pass, including a pass
    /// driven from inside `-eval` ([`Runtime::pump_scheduler`]).
    ///
    /// `fun` must be a fun term built on a heap that outlives the process (the runtime's
    /// arena or a pooled BIF heap — both are pointer-stable and never reset in Milestone 1).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when `fun` is not a fun term (the caller — the native
    /// `erlang:spawn/*` floor — checks this first and reports the oracle's `badarg`, so a
    /// non-fun here is a programming error and never a wrong process), plus the process
    /// table's allocation failure.
    pub fn spawn_fun_process(&mut self, fun: Eterm, args: Vec<Eterm>) -> Result<ProcessId> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        self.boot_if_needed()?;
        let pid = self.processes.spawn_default()?;
        self.runner_state.spawn_fun(pid, fun, args);
        self.enqueue_process(pid)?;
        Ok(pid)
    }

    /// Runs a **bounded** scheduler pass from inside the running code — the `-eval`
    /// evaluator's `receive` pump.
    ///
    /// It performs exactly [`Runtime::run_pass`]'s move-out dance (the process table and
    /// the runner state leave the runtime so the runner can hold `&mut Runtime` while the
    /// scheduler holds `&mut ProcessTable`), with two differences that matter to a caller
    /// that is *not* the entry point:
    ///
    /// * the slice count is always bounded (`max_slices`), because the caller is the
    ///   evaluator's `receive` and must never block (or hang) forever;
    /// * the wake list is absorbed **first** ([`Scheduler::resume_woken`]), so a process a
    ///   send made *outside* a scheduler slice woke is enqueued and run — the
    ///   send→wake→resume path a `receive` waiting for a spawned process's reply needs.
    ///
    /// The runtime's scheduler and runner state persist across calls (they are moved out
    /// and put back), so a pump resumes exactly where the previous one left off.
    ///
    /// Returns the number of slices that ran **plus** the port messages a live port
    /// delivered into their owner's mailbox (see [`Runtime::scheduler_pass`]); `0` means
    /// neither happened. The caller — the evaluator's `receive` — treats a non-zero answer
    /// as "the mailbox may have changed, rescan it", so a port message is picked up exactly
    /// like a message another process sent.
    ///
    /// # Errors
    ///
    /// * a typed diagnostic when an enqueued pid has no registered engine;
    /// * the process table's errors while terminating a process or delivering a message
    ///   between processes;
    /// * the port poller's/driver's failures and a nested borrow of the port table (a typed
    ///   capability gap).
    pub(crate) fn pump_scheduler(&mut self, max_slices: u64) -> Result<u64> {
        let (ran, delivered) = self.scheduler_pass(Some(max_slices), true)?;
        Ok(ran + delivered as u64)
    }

    /// Appends a process created by [`Runtime::spawn_process`] to the scheduler's run
    /// queue and marks it `Runnable`.
    ///
    /// [`Runtime::spawn_process`] already does this, so a caller needs it only to put a
    /// process it took off the queue **back** on it — for instance a receiver woken by a
    /// message delivered from outside a scheduler pass. Enqueuing a pid that is already
    /// queued is a no-op, so a caller cannot accidentally run one process twice per round.
    ///
    /// # Errors
    ///
    /// A typed diagnostic when `pid` is not a process this runtime spawned (there is no
    /// entry point to run) — never a silent no-op that would hide the mistake.
    pub fn enqueue_process(&mut self, pid: ProcessId) -> Result<()> {
        if !self.runner_state.contains(pid) {
            return Err(error::unknown_process_entry(pid));
        }
        if let Some(process) = self.processes.get_mut(pid) {
            if process.flags().in_run_queue() {
                return Ok(());
            }
            process.flags_mut().set_in_run_queue(true);
            process.set_state(ProcessState::Runnable);
        }
        self.scheduler.enqueue(pid);
        Ok(())
    }

    /// Runs **one** scheduler slice (one process, once), returning how many slices ran
    /// (`0` or `1`).
    ///
    /// This is the step hook for observing a run in progress: the scheduler and its run
    /// queue persist, so `run_one_slice` then [`Runtime::run_scheduler`] continues where
    /// the slice left off.
    ///
    /// # Errors
    ///
    /// As [`Runtime::run_scheduler`].
    pub fn run_one_slice(&mut self) -> Result<u64> {
        self.run_slices(1)
    }

    /// Runs at most `max_slices` scheduler slices, returning how many ran.
    ///
    /// # Errors
    ///
    /// As [`Runtime::run_scheduler`].
    pub fn run_slices(&mut self, max_slices: u64) -> Result<u64> {
        self.run_pass(Some(max_slices))
    }

    /// Runs the scheduler until the run queue is empty, returning the number of slices
    /// executed.
    ///
    /// A process blocked on an empty `receive` leaves the queue and simply stays
    /// `Waiting`; a message delivered to it **during** a slice is absorbed at the end of
    /// that slice (the scheduler's `resume_woken`), so a single `run_scheduler` call
    /// carries a send→wake→resume through to completion. A process that is waiting for a
    /// message nobody will send simply stays `Waiting` and the loop ends — it never hangs.
    ///
    /// # Errors
    ///
    /// * a typed diagnostic when an enqueued pid has no registered engine;
    /// * the process table's allocation/propagation errors while terminating a process
    ///   or delivering a message between processes.
    ///
    /// A raise *inside* a process is not an error here: it terminates that process with
    /// the raised reason (observable through [`Runtime::process_exit`]) and the pass
    /// continues.
    pub fn run_scheduler(&mut self) -> Result<u64> {
        self.run_pass(None)
    }

    /// The `x(0)` a process spawned with [`Runtime::spawn_process`] returned, once it has
    /// run to its `return`.
    ///
    /// A process that is still running, blocked, or finished by raising has no recorded
    /// result (`None`); see [`Runtime::process_exit`] for the failing case.
    pub fn process_result(&self, pid: ProcessId) -> Option<Eterm> {
        self.runner_state.result(pid)
    }

    /// The exit reason a process spawned with [`Runtime::spawn_process`] finished with,
    /// once it **raised**.
    ///
    /// A normally-returning process records no exit reason here (its termination is
    /// `normal` and its value is [`Runtime::process_result`]).
    pub fn process_exit(&self, pid: ProcessId) -> Option<Eterm> {
        self.runner_state.exit(pid)
    }

    /// The number of processes waiting in the scheduler's run queue.
    pub fn run_queue_len(&self) -> usize {
        self.scheduler.run_queue().len()
    }

    /// Runs a scheduler pass: the shared dance every `run_*` entry performs.
    ///
    /// The **process table** and the **runner state** are moved out of the runtime, so the
    /// runner can hold `&mut Runtime` while the scheduler holds `&mut ProcessTable` — two
    /// borrows that cannot coexist inside one value (see [`crate::runner`]). Both go back
    /// when the pass ends, whatever its outcome.
    ///
    /// Returns the number of slices that ran; the port half (see
    /// [`Runtime::scheduler_pass`]) is not counted here.
    ///
    /// # Errors
    ///
    /// As [`Runtime::scheduler_pass`].
    fn run_pass(&mut self, max_slices: Option<u64>) -> Result<u64> {
        self.scheduler_pass(max_slices, false).map(|(ran, _)| ran)
    }

    /// Runs a scheduler pass **and drives the port poller**: the shared body of every
    /// `run_*` entry and of [`Runtime::pump_scheduler`].
    ///
    /// The move-out dance is [`Runtime::run_pass`]'s (the runner needs `&mut Runtime` while
    /// the scheduler needs `&mut ProcessTable`). On top of it this adds the runtime's
    /// **port half** — the loop that lets a live port wake a blocked process:
    ///
    /// 1. run the scheduler until the run queue drains (bounded by `max_slices` when given);
    /// 2. with no port live, stop — a runtime that never opened a port pays nothing;
    /// 3. otherwise poll the runtime's **one persistent** port poller for at most
    ///    [`PORT_POLL_SLICE`](crate::port::PORT_POLL_SLICE), run the port tasks readiness
    ///    scheduled, and **deliver** every `{Port, {data, …}}`/`{Port, {exit_status, …}}`
    ///    message into its **owning process's mailbox**
    ///    ([`Runtime::deliver_port_messages`], which uses `beam_process`'s own deliver path);
    /// 4. a delivery is what wakes a receiver blocked in a compiled `receive`, so it
    ///    enqueues the woken process ([`Scheduler::resume_woken`]) and the loop runs the
    ///    scheduler again — until nothing is delivered and the queue is empty.
    ///
    /// A round therefore either runs a slice or delivers a port message; when neither
    /// happens the pass is quiescent and the loop ends (never a busy spin — the poll itself
    /// is bounded by [`PORT_POLL_SLICE`](crate::port::PORT_POLL_SLICE)).
    ///
    /// `resume_first` absorbs the table's pending wake list **before** the first slice — the
    /// behaviour [`Runtime::pump_scheduler`] needs (a send made *outside* a scheduler slice
    /// must be enqueued), which [`Runtime::run_pass`] does not do (its `run_ready` absorbs
    /// wakes at each slice's end anyway).
    ///
    /// Returns `(slices that ran, port messages delivered)`.
    ///
    /// # Errors
    ///
    /// * a typed diagnostic when an enqueued pid has no registered engine;
    /// * the process table's errors while terminating a process or delivering a message
    ///   between processes;
    /// * the port poller's/driver's failures; a nested borrow of the port table is a typed
    ///   capability gap rather than a panic.
    fn scheduler_pass(
        &mut self,
        max_slices: Option<u64>,
        resume_first: bool,
    ) -> Result<(u64, usize)> {
        self.boot_if_needed()?;
        let mut table = self.take_processes();
        let mut scheduler = std::mem::replace(&mut self.scheduler, Scheduler::new());
        let mut state = std::mem::take(&mut self.runner_state);
        // A wake produced *outside* a scheduler slice before this pass started (a `-eval`
        // send, say) must be enqueued first when the caller asked for it.
        if resume_first {
            scheduler.resume_woken(&mut table);
        }
        let mut ran: u64 = 0;
        let mut delivered: usize = 0;
        let outcome: Result<()> = loop {
            // `run_ready` also stops once its own budget is spent; the loop's budget counts
            // every slice across the rounds, so a bounded pass never overshoots.
            let budget = max_slices.map(|max| max.saturating_sub(ran));
            if budget == Some(0) {
                break Ok(());
            }
            let slices = {
                let mut runner = RuntimeRunner::new(self, &mut state);
                scheduler.run_ready(&mut table, &mut runner, budget)
            };
            match slices {
                Ok(executed) => ran += executed,
                Err(err) => break Err(err),
            }
            // The port half: with a live port, read what it produced and deliver it into
            // the owning process's mailbox (which may wake a blocked receiver).
            match self.deliver_port_messages(&mut table, crate::port::PORT_POLL_SLICE) {
                Ok(0) => break Ok(()),
                Ok(count) => {
                    delivered += count;
                    // The delivery may have woken a blocked receiver: absorb the wake list so
                    // the next round runs it.
                    scheduler.resume_woken(&mut table);
                }
                Err(err) => break Err(err),
            }
        };
        self.runner_state = state;
        self.scheduler = scheduler;
        self.put_processes(table);
        outcome?;
        Ok((ran, delivered))
    }

    /// Moves the process table out of the runtime for a scheduler pass.
    ///
    /// The placeholder is an equivalent **empty** table sharing the runtime's atom table
    /// (so its signal atoms are the runtime's); it is only ever observed by a runtime-side
    /// accessor during the pass, never by the running code, which sees the real table.
    fn take_processes(&mut self) -> ProcessTable {
        let placeholder = ProcessTable::new(SignalAtoms::from_table(self.code_server.atoms_mut()));
        std::mem::replace(&mut self.processes, placeholder)
    }

    /// Puts back the process table taken by [`Runtime::take_processes`].
    fn put_processes(&mut self, table: ProcessTable) {
        self.processes = table;
    }

    /// Materializes parsed command-line arguments into terms on the shared atom table
    /// and heap (an atom's index can only be assigned here).
    ///
    /// # Errors
    /// Propagates heap allocation failures while building lists.
    pub fn materialize_args(&mut self, args: &[ArgTerm]) -> Result<Vec<Eterm>> {
        let mut out = Vec::with_capacity(args.len());
        for arg in args {
            let term = materialize(arg, &mut self.code_server, &mut self.arena)?;
            out.push(term);
        }
        Ok(out)
    }

    /// `system_info/1` (contract A.8) materialized as a term.
    ///
    /// The term is built on the runtime's **scratch heap**, which is reset at the start
    /// of every `system_info`/`format_term` call: a returned term is valid **until the
    /// next such call**.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an unknown key, plus
    /// the materialization errors of [`InfoValue::to_eterm`].
    pub fn system_info(&mut self, key: &str) -> Result<Eterm> {
        let value = self.system_info_value(key)?;
        self.info_heap.reset();
        let Runtime {
            info_heap,
            code_server,
            ..
        } = self;
        value.to_eterm(info_heap, code_server.atoms())
    }

    /// `system_info/1` as a heap-independent value — the testable source of truth.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an unknown key.
    pub fn system_info_value(&self, key: &str) -> Result<InfoValue> {
        info::system_info_value(key, self)
    }

    /// Evaluates a general Erlang expression (contract A.6).
    ///
    /// The expression is parsed and evaluated by the `erl_eval`-style tree-walker in
    /// [`crate::eval`], dispatched through the same [`RuntimeContext`] the vertical slice
    /// uses — so arithmetic, comparison and every call reach the `beam_bifs` registry
    /// (or the load-on-demand stdlib) with their exact `erl` semantics. The result is
    /// built on the process heap, so it stays valid for the runtime's lifetime.
    ///
    /// # Errors
    ///
    /// A diagnostic for an expression outside the grammar ([`crate::error::bad_eval`],
    /// a capability gap), whatever a called BIF/module raised, a raise of the evaluated
    /// expression itself (including the uncatchable `erlang:halt/0,1`), or a documented
    /// Milestone-1 gap.
    pub fn eval(&mut self, expr: &str) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(&mut *self);
        crate::eval::eval(&mut ctx, expr)
    }

    /// Prints `term` through the registered `io:format/2` `~p` printer.
    ///
    /// `module` selects the atom table the printer resolves atoms against: the loaded
    /// module's (the one that produced the term) when given, the runtime's shared table
    /// otherwise. Like [`Runtime::system_info`], the format/argument lists are built on
    /// the scratch heap, so the returned text — not any term — is the result.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a term outside the
    /// Milestone-1 printable set; the heap's allocation failure otherwise.
    pub fn format_term(&mut self, term: Eterm, module: Option<&Module>) -> Result<String> {
        self.format_with(term, module, format::FORMAT)
    }

    /// Like [`Runtime::format_term`] but with the `~w` control (no string rule).
    ///
    /// # Errors
    ///
    /// As [`Runtime::format_term`].
    pub fn format_term_w(&mut self, term: Eterm, module: Option<&Module>) -> Result<String> {
        self.format_with(term, module, format::FORMAT_W)
    }

    /// The one printer: renders `term` with `control` on the scratch heap.
    fn format_with(
        &mut self,
        term: Eterm,
        module: Option<&Module>,
        control: &[u8],
    ) -> Result<String> {
        self.info_heap.reset();
        let Runtime {
            info_heap,
            code_server,
            registry,
            self_pid,
            ..
        } = self;
        let table = match module {
            Some(module) => &module.atom_table,
            None => code_server.atoms(),
        };
        format::term_to_string_with(registry, info_heap, table, *self_pid, term, control)
    }

    /// Boots on demand (idempotent).
    fn boot_if_needed(&mut self) -> Result<()> {
        if self.booted {
            Ok(())
        } else {
            self.boot()
        }
    }
}

/// Materializes one parsed argument term (the [`Runtime::materialize_args`] walk).
fn materialize(arg: &ArgTerm, code_server: &mut CodeServer, arena: &mut MinimalContext) -> Result<Eterm> {
    Ok(match arg {
        ArgTerm::Int(value) => make_small(*value),
        ArgTerm::Atom(name) => make_atom(code_server.atoms_mut().intern(name)),
        ArgTerm::Str(text) => charlist(text, arena)?,
        ArgTerm::List(elements) => {
            let mut terms = Vec::with_capacity(elements.len());
            for element in elements {
                terms.push(materialize(element, code_server, arena)?);
            }
            terms::make_list(arena, &terms)?
        }
    })
}

/// `text` as an Erlang string (a char list) built on the arena.
fn charlist(text: &str, arena: &mut MinimalContext) -> Result<Eterm> {
    let elems: Vec<Eterm> = text
        .chars()
        .map(|c| make_small(i64::from(c as u32)))
        .collect();
    terms::make_list(arena, &elems)
}

#[cfg(test)]
mod tests {
    use beam_term::{make_atom, make_small, signed_value};

    use super::*;
    use crate::config::Action;

    /// The committed oracle fixture (reused by relative path — never copied).
    pub(crate) const SLICE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    );

    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    #[test]
    fn boot_is_idempotent_and_reports_itself() {
        let mut rt = runtime();
        assert!(!rt.is_booted());
        rt.boot().expect("boot");
        assert!(rt.is_booted());
        rt.boot().expect("booting twice is a no-op");
        assert!(rt.is_booted());
        assert_eq!(rt.preloaded().len(), boot::PRELOADED_MODULE_NAMES.len());
        assert!(rt.preloaded().is_preloaded("erlang"));
        assert_eq!(rt.config().action, Action::Boot);
    }

    #[test]
    fn a_fresh_runtime_has_core_atoms_and_one_process() {
        let rt = runtime();
        assert!(rt.atoms().lookup("unix").is_some());
        assert!(rt.atoms().lookup("linux").is_some());
        assert_eq!(rt.processes().len(), 1);
        assert_eq!(
            rt.system_info_value("process_count").expect("count"),
            InfoValue::Int(1)
        );
    }

    #[test]
    fn loads_and_caches_a_module_in_the_shared_table() {
        let mut rt = runtime();
        let module = rt.load_module(SLICE_BEAM).expect("load slice.beam");
        assert_eq!(module.atom_name(module.name), Some("slice"));
        assert_eq!(rt.loaded_modules(), vec!["slice".to_string()]);
        assert!(rt.module("slice").is_some());
        assert!(rt.module("nope").is_none());
        // The shared table knows the module's atoms.
        assert!(rt.atoms().lookup("pick").is_some());
        rt.load_module(SLICE_BEAM).expect("reloading replaces");
        assert_eq!(rt.loaded_modules().len(), 1);
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let mut rt = runtime();
        assert!(rt.load_module("/nonexistent/nope.beam").is_err());
        assert!(rt
            .run_function("/nonexistent/nope.beam", "f", 0, &[])
            .is_err());
    }

    #[test]
    fn unknown_function_and_arity_are_diagnostics() {
        let mut rt = runtime();
        let error = rt
            .run_function(SLICE_BEAM, "nope", 0, &[])
            .expect_err("not exported");
        assert!(error.to_string().contains("slice:nope/0"), "{error}");
        let error = rt
            .run_function(SLICE_BEAM, "add", 1, &[make_small(1)])
            .expect_err("wrong arity");
        assert!(error.to_string().contains("slice:add/1"), "{error}");
    }

    #[test]
    fn the_slice_runs_through_the_real_context() {
        let mut rt = runtime();
        let sum = rt
            .run_function(SLICE_BEAM, "add", 2, &[make_small(2), make_small(3)])
            .expect("add/2");
        assert_eq!(signed_value(sum), 5);
        let fib = rt
            .run_function(SLICE_BEAM, "fib", 1, &[make_small(10)])
            .expect("fib/1");
        assert_eq!(signed_value(fib), 55);
    }

    #[test]
    fn the_slice_takes_an_atom_argument_from_the_shared_table() {
        let image = beam_loader::load_file(SLICE_BEAM).expect("load slice.beam");
        let a = make_atom(image.atom_table.lookup("a").expect("a is interned"));
        let mut rt = runtime();
        let mut sink = beam_bifs::NullSink;
        let picked = rt
            .run_module(&image, "pick", 1, &[a], &mut sink)
            .expect("pick(a)");
        assert_eq!(signed_value(picked), 1);
    }

    #[test]
    fn system_info_round_trips_through_the_scratch_heap() {
        let mut rt = runtime();
        let wordsize = rt.system_info("wordsize").expect("wordsize");
        assert_eq!(signed_value(wordsize), std::mem::size_of::<usize>() as i64);
        assert!(matches!(
            rt.system_info("nonsense"),
            Err(beam_common::BeamError::BadArg)
        ));
        let version = rt.system_info("version").expect("version");
        assert!(beam_term::is_list(version));
        let again = rt.system_info("wordsize").expect("wordsize again");
        assert_eq!(signed_value(again), std::mem::size_of::<usize>() as i64);
        let five = rt.eval("2+3").expect("2+3");
        assert_eq!(rt.format_term(five, None).expect("printable"), "5\n");
    }

    #[test]
    fn eval_dispatches_through_the_bif_registry() {
        let mut rt = runtime();
        assert_eq!(signed_value(rt.eval("2+3").expect("2+3")), 5);
        assert_eq!(signed_value(rt.eval("2*3+4").expect("2*3+4")), 10);
        assert_eq!(signed_value(rt.eval("(1+2)*3").expect("(1+2)*3")), 9);
        // A bare atom is a value; only something outside the grammar is an error.
        let atom = rt.eval("nope").expect("an atom evaluates to itself");
        assert_eq!(rt.atom_name_of(atom), Some("nope"));
        assert!(rt.eval("1 +").is_err());
    }

    #[test]
    fn format_term_uses_the_registered_printer() {
        let mut rt = runtime();
        assert_eq!(rt.format_term(make_small(7), None).expect("printable"), "7\n");
        assert_eq!(
            rt.format_term(beam_term::NIL, None).expect("printable"),
            "[]\n"
        );
        // `~w` does not apply the string rule.
        assert_eq!(
            rt.format_term_w(beam_term::NIL, None).expect("printable"),
            "[]\n"
        );
    }

    #[test]
    fn materializes_parsed_argument_terms() {
        let mut rt = runtime();
        let args = vec![
            ArgTerm::Int(-7),
            ArgTerm::Atom("a".to_string()),
            ArgTerm::Str("hi".to_string()),
            ArgTerm::List(vec![ArgTerm::Int(1), ArgTerm::Atom("b".to_string())]),
        ];
        let terms = rt.materialize_args(&args).expect("materialize");
        assert_eq!(signed_value(terms[0]), -7);
        assert_eq!(rt.atom_name_of(terms[1]), Some("a"));
        let chars = terms::list_elements(terms[2]).expect("a char list");
        assert_eq!(signed_value(chars[0]), i64::from(b'h'));
        assert_eq!(terms::list_elements(terms[3]).expect("a list").len(), 2);
    }

    #[test]
    fn module_files_are_found_through_the_code_path() {
        let dir = Path::new(SLICE_BEAM).parent().expect("a parent directory");
        let config = RuntimeConfig::default().with_code_path(dir);
        let mut rt = Runtime::new(config).expect("runtime");
        assert_eq!(rt.find_module_file("slice"), Some(PathBuf::from(SLICE_BEAM)));
    }
}
