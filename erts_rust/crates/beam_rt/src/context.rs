//! The concrete [`ExecutionContext`] glue: the engine's seam over the runtime core.
//!
//! The Stage-1 engine knows nothing about BIFs, heaps, processes or ETS — it calls
//! `call_bif`/`ensure_heap`/`alloc_words`/`apply_fun` on the trait and lets the host
//! decide (see `../beam_engine/src/context.rs`). This module is the host:
//! [`RuntimeContext`] borrows the [`Runtime`] (its BIF registry, the shared
//! [`AtomTable`](beam_term::AtomTable), the process heap, the calling [`ProcessId`], the
//! ETS [`TableRegistry`] and the output sink) and adapts them onto the engine.
//!
//! It is the **only** place the engine's seam meets the Stage-5 core; that keeps the
//! Stage-1/Stage-5 dependency DAG acyclic (`../CONTEXT.md`).
//!
//! # One context, three dispatch routes
//!
//! The engine routes `call_ext*` **and** `apply`/`apply_last` through
//! [`ExecutionContext::call_bif`], and `call_fun`/`call_fun2` through
//! [`ExecutionContext::apply_fun`]. So [`RuntimeContext::call_bif`] serves, in order:
//!
//! 1. the **runtime-native floor** ([`crate::native`]: `halt`, `display`,
//!    `io:put_chars`, `erlang:apply`, `init:get_argument`);
//! 2. the **`beam_bifs` registry** (the `bif.tab` surface);
//! 3. an **external module call**, loaded on demand from the code path through the
//!    [`CodeServer`](crate::CodeServer) and run with `self` as the context — the
//!    recursion that makes the runtime a real multi-module host.
//!
//! # The shared atom table
//!
//! A [`BifContext`] takes *one* [`AtomTable`](beam_term::AtomTable). The engine resolves
//! atoms — including the ones inside `Module` code — against the **loaded module's**
//! table (`beam_engine`'s `cmp(.., &module.atom_table)`). Because every module the
//! [`CodeServer`](crate::CodeServer) loads is interned into the runtime's **one shared**
//! table, handing the BIFs that same table is exactly right: an atom a BIF builds is an
//! index the engine can compare and the printer can name, for **every** loaded module.
//!
//! # Interning new atoms (the predicted-index commit)
//!
//! Some BIFs (`binary_to_term/1`, `list_to_atom/1`, …) must be able to add a **new** atom
//! to that same shared table, but the [`BifContext`] only borrows it *immutably* for index
//! resolution. Since this crate is `#![deny(unsafe_code)]`, the interner cannot hold a
//! second, mutable borrow of the one table. The [`AtomInterner`] hook solves this without
//! aliasing: [`PendingInterner`] records the names a BIF interns during a call and
//! **predicts** each index as `base + <names already recorded>` (interning is append-only
//! and the runtime is single-threaded, so the prediction is exact); after the BIF returns
//! — releasing the table borrow — [`RuntimeContext::call_registered`] commits the recorded
//! names into the shared table in the same order, so each lands at exactly the predicted
//! index. A name seen twice in one call is deduplicated and gets the same index.
//!
//! # No collector yet
//!
//! [`ensure_heap`](ExecutionContext::ensure_heap) and
//! [`alloc_words`](ExecutionContext::alloc_words) are served by the runtime's
//! pointer-stable [`MinimalContext`](beam_engine::MinimalContext) arena. That arena
//! treats the heap as unbounded, so `ensure_heap` never reports a short heap — there is
//! no GC in the engine yet (`../beam_engine` Known Issues). The production collector is
//! Stage 4 (`beam_gc`), which will run *here* and may move terms.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use beam_bifs::{AtomInterner, BifContext, FunCaller, OutputSink};
use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext, Machine};
use beam_loader::Module;
use beam_message::Queue;
use beam_process::{ProcessId, ProcessTable};
use beam_term::{Atom, Eterm};

use crate::error;
use crate::mailbox::MailboxState;
use crate::port::RuntimePorts;
use crate::runtime::Runtime;

/// A cloneable, shared `io:format` destination.
///
/// A [`RuntimeContext`] must be able to hand the **same** sink to a nested context when
/// a `fun`-taking BIF applies its fun (see [`FunBridge`]) while the outer call still
/// holds it — but a borrowed `&mut dyn OutputSink` cannot be duplicated. The sink
/// therefore lives behind an `Rc`-shared, interior-mutable cell: every context owns its
/// own [`SharedSink`] *value* wrapping a clone of the same `Rc`, and the (single-threaded)
/// writes are serialized by the `RefCell`, so there is no Rust aliasing — this crate is
/// `#![deny(unsafe_code)]`.
///
/// The shared cell is a `dyn SinkCell` rather than a `RefCell<dyn OutputSink>` on purpose:
/// it keeps `SharedSink<'a>` **covariant** in `'a`, so a nested context can reuse a sink
/// borrowed for a *longer* lifetime — exactly what [`FunBridge::call_fun`] does when it
/// reborrows the runtime for a short nested call.
#[derive(Clone)]
pub(crate) struct SharedSink<'a>(Rc<dyn SinkCell + 'a>);

/// The interior-mutable cell behind a [`SharedSink`].
///
/// A trait (not a generic struct) so the object type carries the sink's lifetime — and
/// nothing else — which is what makes [`SharedSink`] covariant in `'a` (a `RefCell` over a
/// lifetime-parameterized type would be invariant).
trait SinkCell {
    /// Writes `s` through the borrowed sink.
    fn write(&self, s: &str) -> Result<()>;
}

impl<'a> SinkCell for RefCell<&'a mut dyn OutputSink> {
    fn write(&self, s: &str) -> Result<()> {
        let mut sink = self.borrow_mut();
        sink.write_str(s)
    }
}

impl<'a> SharedSink<'a> {
    /// Wraps a borrowed sink into a shared handle.
    fn new(sink: &'a mut dyn OutputSink) -> SharedSink<'a> {
        SharedSink(Rc::new(RefCell::new(sink)))
    }
}

impl OutputSink for SharedSink<'_> {
    fn write_str(&mut self, s: &str) -> Result<()> {
        self.0.write(s)
    }
}

/// The engine's host: the runtime, borrowed for the duration of one run, plus the
/// output sink that run writes to.
///
/// A `RuntimeContext` with no sink ([`RuntimeContext::new`]) writes `io:format` output
/// to the process's stdout (`beam_bifs`' default); [`RuntimeContext::with_output`]
/// redirects it — that is how [`Runtime::run_module`] captures output.
pub struct RuntimeContext<'a> {
    /// The runtime: registry, shared atom table, heaps, pid, ETS tables.
    ///
    /// `pub(crate)` for the runtime-native BIF floor ([`crate::native`]), which is a
    /// sibling module of this one and therefore needs explicit visibility.
    pub(crate) runtime: &'a mut Runtime,
    /// The `io:format`/`io:put_chars`/`display` destination; stdout when unset.
    ///
    /// A **shared** handle (see [`SharedSink`]) so the *same* sink can be handed to a
    /// nested context when a `fun`-taking BIF applies its fun ([`FunBridge`]).
    pub(crate) output: Option<SharedSink<'a>>,
    /// The process the context runs **as** — the mailbox owner, and the pid
    /// `erlang:self/0` reports.
    ///
    /// It is the runtime's own [`self_pid`](Runtime::self_pid) unless the context was
    /// built with [`RuntimeContext::for_process`].
    mailbox_pid: ProcessId,
    /// A **caller-supplied** process table to resolve that process against, if any.
    ///
    /// `None` means "use the runtime's own [`processes`](Runtime::processes)". The
    /// scheduler pass runs a process whose `Process` lives in a `ProcessTable` it owns
    /// separately from `&mut Runtime` (it cannot hand the same table to both), so the
    /// context must be able to resolve a mailbox against either source — see
    /// [`RuntimeContext::for_process`].
    mailbox_table: Option<&'a mut ProcessTable>,
}

impl<'a> RuntimeContext<'a> {
    /// A context over `runtime`, with the default (stdout) output sink, running as the
    /// runtime's own process.
    pub fn new(runtime: &'a mut Runtime) -> RuntimeContext<'a> {
        let mailbox_pid = runtime.self_pid;
        RuntimeContext {
            runtime,
            output: None,
            mailbox_pid,
            mailbox_table: None,
        }
    }

    /// A context over `runtime` whose program output goes to `sink`.
    pub fn with_output(runtime: &'a mut Runtime, sink: &'a mut dyn OutputSink) -> RuntimeContext<'a> {
        let mailbox_pid = runtime.self_pid;
        RuntimeContext {
            runtime,
            output: Some(SharedSink::new(sink)),
            mailbox_pid,
            mailbox_table: None,
        }
    }

    /// A context that runs **as `pid`**, resolving that process and its mailbox against
    /// the caller-supplied `table` instead of the runtime's own
    /// [`processes`](Runtime::processes).
    ///
    /// This is the seam the scheduler pass builds on: a `ProcessRunner` receives the
    /// `ProcessTable` it schedules from separately from the `&mut Runtime` it needs for
    /// BIF dispatch (it cannot lend the same table to both), so it constructs a context
    /// this way. The mailbox **scan state** is unaffected — it always lives in
    /// [`Runtime::mailboxes`](crate::runtime::Runtime), keyed by `pid`, because a
    /// suspended receive must resume against exactly the position it stopped at.
    ///
    /// `table` must contain `pid`; a receive-seam call for a pid it does not hold is the
    /// typed [`crate::error::no_mailbox`] capability gap, never a panic. A `send` through
    /// such a context delivers into `table` (the table `pid` belongs to), so a target pid
    /// living in a *different* table is reported as the table's typed not-found error.
    pub fn for_process(
        runtime: &'a mut Runtime,
        table: &'a mut ProcessTable,
        pid: ProcessId,
    ) -> RuntimeContext<'a> {
        RuntimeContext {
            runtime,
            output: None,
            mailbox_pid: pid,
            mailbox_table: Some(table),
        }
    }

    /// The pid this context runs as (the mailbox owner and `erlang:self/0`).
    pub fn pid(&self) -> ProcessId {
        self.mailbox_pid
    }

    /// The process table a receive/send resolves against: the caller-supplied one when
    /// there is one, else the runtime's own.
    fn process_table_mut(&mut self) -> &mut ProcessTable {
        match self.mailbox_table {
            Some(ref mut table) => &mut **table,
            None => &mut self.runtime.processes,
        }
    }

    /// The running process's mailbox **message queue** and **scan state**, borrowed
    /// together.
    ///
    /// The queue lives in the `Process` (in the caller-supplied table or the runtime's
    /// own); the scan state lives in [`Runtime::mailboxes`](crate::runtime::Runtime).
    /// They are disjoint, so both are handed out mutably at once. A pid with no process
    /// is the typed [`crate::error::no_mailbox`] capability gap.
    fn mailbox_parts(&mut self) -> Result<(&mut Queue, &mut MailboxState)> {
        let pid = self.mailbox_pid;
        let Runtime {
            processes,
            mailboxes,
            ..
        } = &mut *self.runtime;
        // `table` and `mailboxes`/`processes` are disjoint places, so the borrows below
        // do not conflict.
        let table: &mut ProcessTable = match self.mailbox_table {
            Some(ref mut table) => &mut **table,
            None => processes,
        };
        // A pid with no process is the capability gap *before* any scan state is
        // created for it, so a ghost pid never accumulates a stale entry.
        let process = table.get_mut(pid).ok_or_else(|| error::no_mailbox(pid))?;
        let state = mailboxes.entry(pid).or_default();
        Ok((process.mailbox_mut(), state))
    }

    /// Heap words still available to the running code.
    ///
    /// Milestone-1's arena is unbounded (no GC yet), so this reports
    /// `usize::MAX` — see the module docs.
    pub fn heap_available(&self) -> usize {
        self.runtime.arena_available()
    }

    /// The runtime's long-lived `-eval` heap.
    ///
    /// The evaluator builds a term here when the engine arena is not enough — a
    /// non-byte-aligned bitstring, which [`beam_codec::term::make_bitstring`] writes
    /// through a [`beam_alloc::Heap`]. That heap never moves its terms and
    /// Milestone-1 never resets it, so the term stays valid for the runtime's
    /// lifetime (see [`crate::runtime::Runtime::eval_heap`]).
    pub(crate) fn eval_heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.runtime.eval_heap
    }

    /// Runs `module`'s exported `function/arity` with `args`, on this context.
    ///
    /// This is the direct entry point (no BIF/registry indirection): it is what
    /// [`Runtime::run_function`]/[`Runtime::run_module`] use for the top-level run and
    /// what [`ExecutionContext::call_bif`] uses for an external module call.
    ///
    /// # Errors
    ///
    /// A diagnostic naming `module:function/arity` when the function is not in the
    /// module's atom table or is not exported, plus any error the running code raises.
    pub(crate) fn run_export(
        &mut self,
        module: &Module,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let name = module.atom_name(module.name).unwrap_or("<unnamed>");
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or_else(|| error::unknown_function(name, function, arity))?;
        if module.export(function_atom, arity).is_none() {
            return Err(error::unknown_function(name, function, arity));
        }
        Machine::new().run_export(module, function_atom, arity, args, self)
    }

    /// Dispatches a `beam_bifs`-registered BIF over a [`BifContext`] built from the
    /// runtime's shared state and this context's output sink.
    ///
    /// The context is handed a [`PendingInterner`] so a BIF that builds a *new* atom
    /// (`binary_to_term/1`, `list_to_atom/1`, …) can intern it into the one shared table
    /// without aliasing it: the interner records the names, the BIF runs against the
    /// immutable table, and the names are committed (in order, at their predicted
    /// indices) once the BIF has returned and released the borrow — see the module docs.
    ///
    /// # Why every resource is a local
    ///
    /// The `fun`-taking `lists`/`maps` BIFs call back into the runtime through
    /// [`FunBridge`] (the [`FunCaller`] seam), which needs `&mut Runtime`. The
    /// [`BifContext`] therefore holds **no** borrow of the runtime: the registry, the
    /// atom table and the tables are cheap `Rc` clones (the atom handle dereferences to
    /// the shared table without borrowing the server), the heap comes from the BIF heap
    /// pool, and the output sink is a shared [`SharedSink`] handle cloned out of this
    /// context (see below). The only borrow of the runtime is the bridge's.
    ///
    /// A **nested** call (inside a fun a BIF applied) re-enters here through the bridge:
    /// it takes its own heap, shares the atom table through another `Rc` handle and shares
    /// the caller's sink through a [`SharedSink`] clone, and — because the outer call did
    /// **not** take the ETS cell (see [`Self::reentrancy_capable`]) — borrows the shared
    /// tables and reaches the outer tables' `ets:*` operations.
    fn call_registered(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // Cheap handles that do NOT borrow the runtime: the runtime stays free for the
        // fun-caller bridge.
        let registry = Rc::clone(&self.runtime.registry);
        let atoms = self.runtime.code_server.atoms_handle();
        let tables_handle = Rc::clone(&self.runtime.tables);
        let ports_handle = Rc::clone(&self.runtime.ports);
        let mut heap = self.runtime.take_bif_heap()?;
        // `self_pid` is the pid this context runs as: `erlang:self/0` reports it, and a
        // `receive` in a fun a BIF applies resolves its mailbox against it.
        let self_pid = self.mailbox_pid;
        // `base` is the shared table's length before the call: the index the first new
        // name gets, because nothing else interns while this single-threaded call runs.
        let mut pending = PendingInterner::new(atoms.len());
        let result = {
            let RuntimeContext {
                runtime,
                output,
                mailbox_pid,
                mailbox_table,
            } = self;
            // Re-entry happens only when the BIF *applies a fun argument* (the fun-taking
            // `lists:*`/`maps:*` BIFs): the applied fun can call back into the runtime and
            // reach `ets:*`. Borrowing the shared `TableRegistry` for the whole call would
            // leave its `RefCell` taken, so the nested `ets:*` could not borrow it. When an
            // argument is a fun, therefore, do **not** install the tables — the cell stays
            // free for the nested call. Nothing is lost: a BIF that takes a fun scans it
            // with `lists`/`maps` logic and does not touch ETS itself (no M1 BIF both needs
            // ETS *and* applies a fun argument).
            let reentrant = Self::reentrancy_capable(args);
            let mut tables = if reentrant {
                None
            } else {
                // `try_borrow_mut` (never `borrow_mut`): an unexpected outer borrow must
                // degrade to "no ETS", never panic.
                tables_handle.try_borrow_mut().ok()
            };
            // The bridge gets its own clone of the shared sink, so a `fun` a BIF applies
            // writes to the caller's sink (not stdout) — see `SharedSink`/`FunBridge`.
            let mut bridge = FunBridge {
                runtime,
                output: output.clone(),
                mailbox_pid: *mailbox_pid,
                mailbox_table: mailbox_table.as_deref_mut(),
            };
            // The port provider is another cheap handle: it holds the shared `Rc` port
            // registry and a clone of the shared atom-table handle, so `erlang:open_port/2`
            // really starts an OS process and `erlang:port_info/1,2` answers from the
            // runtime's own port table (see `crate::port`).
            let mut ports = RuntimePorts::new(ports_handle, Rc::clone(&atoms));
            let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                .with_atom_interner(&mut pending)
                .with_fun_caller(&mut bridge)
                .with_port_hook(&mut ports);
            if let Some(sink) = output.as_mut() {
                context = context.with_output(sink);
            }
            if let Some(tables) = tables.as_deref_mut() {
                context = context.with_tables(tables);
            }
            registry.call(module, function, arity, &mut context, args)
        };
        // The BIF is done and every borrow released, so commit the names it interned:
        // each lands at the index the interner predicted, keeping the BIF's atom terms
        // (and every printer/comparison that resolves them) consistent.
        for name in pending.into_pending() {
            self.runtime.code_server.atoms_mut().intern(&name);
        }
        self.runtime.put_bif_heap(heap);
        result
    }

    /// Whether a call with `args` can re-enter the runtime by *applying a fun argument*.
    ///
    /// The `fun`-taking `lists`/`maps` BIFs (`lists:map/2`, `lists:foldl/3`, `maps:map/2`,
    /// …) take the fun they apply as a **direct argument** and call it back through
    /// [`FunBridge`]; a BIF with no fun argument never re-enters. [`Self::call_registered`]
    /// uses this to decide whether it may hold the shared ETS cell for the call (see the
    /// limitation documented there).
    fn reentrancy_capable(args: &[Eterm]) -> bool {
        args.iter().any(|arg| terms::is_fun(*arg))
    }
}

/// The `beam_bifs` fun-calling seam: runs a fun term for a `fun`-taking BIF.
///
/// The `fun`-taking `lists`/`maps` BIFs (`lists:map/2`, `lists:foldl/3`, `lists:sort/2`,
/// `maps:map/2`, …) are plain Rust functions and cannot execute BEAM code themselves, so
/// they call their fun through [`BifContext::call_fun`] — this is the concrete
/// [`FunCaller`] the runtime installs. It borrows the whole runtime (the caller's
/// `BifContext` holds only locals and cheap clones, so the runtime is free), builds a
/// nested [`RuntimeContext`] over it and delegates to [`ExecutionContext::apply_fun`]:
/// an `-eval` closure (`uniq` == [`crate::eval::EVAL_FUN_UNIQ`]) is applied through
/// [`crate::eval::apply_eval_closure`], and every other fun is a `.beam` fun applied
/// through `Machine::run_label` — exactly the semantics a direct `call_fun` opcode has.
///
/// It carries a clone of the caller's [`SharedSink`], so a nested `io:format` reaches the
/// caller's sink rather than stdout. It carries **no** [`TableRegistry`] of its own: the
/// outer call intentionally did not take the shared cell (see
/// [`RuntimeContext::reentrancy_capable`]), so the nested context's own
/// [`RuntimeContext::call_registered`] borrows it normally and `ets:*` inside the applied
/// fun reaches the runtime's tables.
struct FunBridge<'a> {
    /// The runtime the fun runs on.
    runtime: &'a mut Runtime,
    /// The caller's shared output sink (stdout when unset), cloned from the outer context.
    output: Option<SharedSink<'a>>,
    /// The pid the outer call runs as, so a `receive` inside the applied fun resolves
    /// the *same* process's mailbox.
    mailbox_pid: ProcessId,
    /// The outer call's externally supplied process table, if any (see
    /// [`RuntimeContext::for_process`]).
    mailbox_table: Option<&'a mut ProcessTable>,
}

impl FunCaller for FunBridge<'_> {
    fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        // A fresh nested context over the same runtime, sharing the caller's output sink
        // so a nested `io:format` reaches it. The ETS cell is left free by the outer call
        // (it is re-entrancy-capable), so the nested `call_registered` borrows it normally.
        // The mailbox identity (pid + table) is inherited, so a `receive` in the fun
        // scans the same process's mailbox as the outer call.
        let mut ctx = RuntimeContext {
            runtime: self.runtime,
            output: self.output.clone(),
            mailbox_pid: self.mailbox_pid,
            mailbox_table: self.mailbox_table.as_deref_mut(),
        };
        ctx.apply_fun(fun, args)
    }
}

/// Interns atom names for the duration of one BIF call, predicting their indices.
///
/// A [`BifContext`] borrows the shared [`AtomTable`](beam_term::AtomTable) immutably for
/// index resolution while the interner must add names to it; this crate is
/// `#![deny(unsafe_code)]`, so the interner cannot hold a coexisting mutable borrow of the
/// same table. Interning is **append-only** and the runtime is **single-threaded**, so a
/// name added during one call always lands at `base + <names recorded so far>` — the
/// interner records the names and predicts that index; [`RuntimeContext::call_registered`]
/// commits them, in order, after the call. A name seen twice in one call is deduplicated
/// and returns the same index.
struct PendingInterner {
    /// The shared table's length when the call began: the first new name's index.
    base: usize,
    /// Names interned during this call, in order (deduplicated).
    pending: Vec<String>,
}

impl PendingInterner {
    /// A pending interner rooted at `base` (the current shared-table length).
    fn new(base: usize) -> PendingInterner {
        PendingInterner {
            base,
            pending: Vec::new(),
        }
    }

    /// The recorded names, in intern order, to be committed by the caller.
    fn into_pending(self) -> Vec<String> {
        self.pending
    }
}

impl AtomInterner for PendingInterner {
    fn intern(&mut self, name: &str) -> Atom {
        if let Some(offset) = self.pending.iter().position(|pending| pending == name) {
            // A duplicate within this call reuses the predicted index.
            return Atom::from_raw((self.base + offset) as u32);
        }
        let index = self.base + self.pending.len();
        self.pending.push(name.to_string());
        Atom::from_raw(index as u32)
    }
}

impl ExecutionContext for RuntimeContext<'_> {
    /// The runtime's unified dispatch — see the module docs.
    ///
    /// 1. The runtime-native floor ([`RuntimeContext::try_native`]).
    /// 2. A registered `beam_bifs` BIF, with the runtime's **shared** atom table.
    /// 3. Otherwise an external module call: the module is loaded on demand from the
    ///    code path and run with `self` as the context. `erlang` is the BIF namespace,
    ///    never a loadable module, so an unknown `erlang:*` call is a typed diagnostic.
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if let Some(result) = self.try_native(module, function, arity, args) {
            return result;
        }
        if self.runtime.registry.contains(module, function, arity) {
            return self.call_registered(module, function, arity, args);
        }
        if module == "erlang" {
            return Err(error::unknown_bif(module, function, arity));
        }
        // The code server adds the system library directories (stdlib, kernel) the first
        // time a module has to be resolved, so an external call reaches the real stdlib
        // without an explicit `-pa`. A miss here is not fatal: `CodeServer::load` below
        // reports it, naming every directory searched.
        let _ = self.runtime.find_module_file(module);
        let loaded = self.runtime.code_server.load(module)?;
        self.run_export(&loaded, function, arity, args)
    }

    /// Ensures `heap_need` words are available in the runtime's arena.
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.runtime
            .ensure_heap(heap_need, live, x_regs, stack)
    }

    /// Heap words still available (see [`RuntimeContext::heap_available`]).
    fn heap_available(&self) -> usize {
        RuntimeContext::heap_available(self)
    }

    /// Allocates `n` raw heap words from the runtime's pointer-stable arena.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.runtime.alloc_words(n)
    }

    /// Applies a fun/closure value.
    ///
    /// A fun built by the `-eval` evaluator (`uniq` == `eval::EVAL_FUN_UNIQ`) is applied
    /// through the evaluator's own store (`Runtime::eval_closures`): the fun may escape
    /// into a loaded module (`lists:map(fun(X) -> X*X end, …)`) which calls back into it,
    /// and the closure body can only be run by the evaluator. Every other fun is a
    /// `.beam` fun: it carries only its `uniq` (per-module) and `index` (the module-level
    /// lambda index); together they identify the [`FunDescriptor`] — hence the defining
    /// module and its entry label — unambiguously across every module the code server
    /// has loaded. The closure body is entered with the **free variables first**,
    /// followed by the call arguments (a lambda's captured variables are its leading
    /// parameters).
    ///
    /// [`FunDescriptor`]: beam_loader::FunDescriptor
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        let uniq = terms::fun_uniq(fun)?;
        if uniq == crate::eval::EVAL_FUN_UNIQ {
            let index = terms::fun_index(fun)? as usize;
            return crate::eval::apply_eval_closure(self, fun, index, args);
        }
        let index = terms::fun_index(fun)?;
        let mut combined = terms::fun_free(fun)?.to_vec();
        combined.extend_from_slice(args);

        let found = self
            .runtime
            .code_server
            .modules()
            .find(|module| descriptor_of(module, uniq, index).is_some())
            .map(Arc::clone)
            .ok_or(BeamError::BadArg)?;
        let descriptor = descriptor_of(&found, uniq, index).ok_or(BeamError::BadArg)?;
        if combined.len() != descriptor.arity as usize {
            return Err(BeamError::BadArity);
        }
        Machine::new().run_label(&found, descriptor.label, descriptor.arity, &combined, self)
    }

    /// `send`: deliver `message` to the pid term `to`.
    ///
    /// `to` is resolved with [`ProcessId::from_eterm`] (a non-pid is [`BeamError::BadArg`])
    /// and the message is copied into the recipient's heap and enqueued by
    /// [`ProcessTable::deliver_simple`] (an unknown pid is the table's typed
    /// `TermNotFound`). The sender is this context's pid, so `self() ! M` reaches the very
    /// mailbox the following `receive` scans.
    ///
    /// A delivery that wakes a blocked recipient reports [`DeliverOutcome::Woke`]; the
    /// wake is **recorded on the process table's wake list** (not lost) and a later
    /// scheduler pass absorbs it by calling `Scheduler::resume_woken`. Milestone 1 has no
    /// scheduler loop, so nothing enqueues the woken process here — which is why a
    /// self-delivery is the `Enqueued` case this crate relies on.
    ///
    /// [`DeliverOutcome::Woke`]: beam_process::DeliverOutcome::Woke
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        let to = ProcessId::from_eterm(to)?;
        let from = self.mailbox_pid.to_eterm();
        // `deliver_simple` pushes a woken recipient onto the table's wake list, so the
        // wake survives for the scheduler pass; the outcome itself needs no action here.
        let _outcome = self.process_table_mut().deliver_simple(to, from, message)?;
        Ok(())
    }

    /// `loop_rec`: the candidate at the current mailbox scan position, `Ok(None)` when
    /// the scan/mailbox is exhausted.
    ///
    /// **Peeks**: the scan position does not move (the following `loop_rec_end` advances
    /// it). Never `unsupported` while the running process exists — an empty mailbox is
    /// simply `Ok(None)`, which makes the compiled code jump to the `wait` that blocks.
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        let (queue, state) = self.mailbox_parts()?;
        Ok(state.next(queue))
    }

    /// `loop_rec_end`: advance the scan past the current candidate.
    fn receive_skip(&mut self) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.skip(queue);
        Ok(())
    }

    /// `remove_message`: unlink the matched candidate and reposition the scan at the
    /// first message.
    fn receive_remove(&mut self) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.remove(queue);
        Ok(())
    }

    /// `timeout`: reset the scan position to the first message.
    fn receive_reset(&mut self) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.reset(queue);
        Ok(())
    }

    /// `recv_marker_reserve`: an opaque marker term for the current scan position.
    ///
    /// On an **empty** queue there is no position to record, so the C `am_undefined` is
    /// returned instead — interned into the one shared atom table so the term is a real,
    /// nameable atom (and, being non-small, makes the following `recv_marker_bind` a
    /// no-op). Otherwise a fresh small-integer marker is returned.
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        let undefined = self.runtime.intern("undefined");
        let (queue, state) = self.mailbox_parts()?;
        Ok(state.reserve(queue, undefined))
    }

    /// `recv_marker_bind`: bind the opaque `marker` to the (internal) reference.
    ///
    /// A non-small marker (the empty-queue `undefined`) is a silent no-op.
    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        let (_queue, state) = self.mailbox_parts()?;
        state.bind(marker, reference);
        Ok(())
    }

    /// `recv_marker_use`: set the scan position to the position `reference`'s marker
    /// recorded (a no-op for an unknown reference).
    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.use_reference(queue, reference);
        Ok(())
    }

    /// `recv_marker_clear`: release `reference`'s marker (a no-op for an unknown one).
    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        let (_queue, state) = self.mailbox_parts()?;
        state.clear(reference);
        Ok(())
    }
}

/// The `FunDescriptor` of the lambda `(uniq, index)` in `module`, if it defines one.
fn descriptor_of(
    module: &Module,
    uniq: u64,
    index: u32,
) -> Option<beam_loader::FunDescriptor> {
    module
        .funs()
        .iter()
        .find(|descriptor| u64::from(descriptor.uniq) == uniq && descriptor.index == index)
        .copied()
}

#[cfg(test)]
mod tests {
    use beam_bifs::{BifRegistry, BytesSink};
    use beam_engine::RunOutcome;
    use beam_term::{atom_val, is_small, make_small, signed_value};

    use super::*;
    use crate::config::RuntimeConfig;

    fn runtime() -> Runtime {
        Runtime::new(RuntimeConfig::default()).expect("the default runtime")
    }

    #[test]
    fn dispatches_a_registered_erlang_bif() {
        let mut rt = runtime();
        let mut ctx = RuntimeContext::new(&mut rt);
        let sum = ctx
            .call_bif("erlang", "+", 2, &[make_small(2), make_small(3)])
            .expect("2 + 3");
        assert!(is_small(sum));
        assert_eq!(signed_value(sum), 5);
        assert_eq!(
            signed_value(
                ctx.call_bif("erlang", "*", 2, &[make_small(6), make_small(7)])
                    .expect("6 * 7")
            ),
            42
        );
    }

    #[test]
    fn a_bif_builds_an_atom_from_the_shared_table() {
        let mut rt = runtime();
        let a = rt.intern("a");
        let mut ctx = RuntimeContext::new(&mut rt);
        let is_atom = ctx
            .call_bif("erlang", "is_atom", 1, &[a])
            .expect("is_atom(a)");
        // `ctx` now owns a shared-sink handle (and therefore a destructor), so the runtime
        // is reached through the context that borrows it rather than the outer binding.
        assert_eq!(
            ctx.runtime.atoms().name(atom_val(is_atom)),
            Some("true"),
            "the atom the BIF built resolves through the shared table"
        );
    }

    #[test]
    fn the_pending_interner_predicts_append_only_indices_and_dedups() {
        let mut interner = PendingInterner::new(10);
        assert_eq!(interner.intern("foo").raw(), 10);
        assert_eq!(interner.intern("bar").raw(), 11);
        // A duplicate within the same call reuses its predicted index.
        assert_eq!(interner.intern("foo").raw(), 10);
        assert_eq!(interner.intern("baz").raw(), 12);
        assert_eq!(interner.into_pending(), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn a_bif_interns_a_new_atom_into_the_shared_table() {
        let mut rt = runtime();
        let name = "a_fresh_atom_zzz";
        assert!(rt.atoms().lookup(name).is_none(), "the name starts unknown");
        let base = rt.atoms().len();
        let mut ctx = RuntimeContext::new(&mut rt);
        let image = terms::make_heap_binary(&mut ctx, name.as_bytes()).expect("a binary");
        let atom = ctx
            .call_bif("erlang", "binary_to_atom", 1, &[image])
            .expect("binary_to_atom/1 interns a new name");
        // The atom the BIF built resolves through the one shared table: the name was
        // committed after the call, at exactly the index the interner predicted.
        // (`ctx` holds a shared-sink handle with a destructor, so the runtime is reached
        // through the context that borrows it.)
        assert_eq!(ctx.runtime.atoms().name(atom_val(atom)), Some(name));
        assert_eq!(ctx.runtime.atoms().len(), base + 1);
    }

    #[test]
    fn an_unknown_erlang_bif_is_a_typed_error_not_a_panic() {
        let mut rt = runtime();
        let mut ctx = RuntimeContext::new(&mut rt);
        // `erlang` is the BIF namespace, never a loadable module.
        let error = ctx
            .call_bif("erlang", "nope", 0, &[])
            .expect_err("not a BIF");
        assert!(error.to_string().contains("erlang:nope/0"), "{error}");
        assert!(ctx
            .call_bif("erlang", "+", 2, &[make_small(1)])
            .is_err(), "wrong arity");
    }

    #[test]
    fn io_format_writes_to_the_installed_sink() {
        let mut rt = runtime();
        let format: Vec<Eterm> = "hi ~w\n".bytes().map(|b| make_small(i64::from(b))).collect();
        let mut sink = BytesSink::new();
        let result = {
            let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
            let format = terms::make_list(&mut ctx, &format).expect("a format list");
            let args = terms::make_list(&mut ctx, &[make_small(7)]).expect("an argument list");
            ctx.call_bif("io", "format", 2, &[format, args])
                .expect("io:format/2")
        };
        assert_eq!(rt.atoms().name(atom_val(result)), Some("ok"));
        assert_eq!(sink.as_str(), "hi 7\n");
    }

    #[test]
    fn ets_inside_an_applied_fun_reaches_the_outer_table() {
        let mut rt = runtime();
        // `lists:map/2` applies its fun argument through `FunBridge`; the `ets:*` calls
        // inside that fun must reach the runtime's shared tables. Before `call_registered`
        // stopped holding the ETS cell for a re-entrancy-capable call, the nested
        // `ets:insert` raised `unsupported_bif` (the cell was already borrowed by the
        // outer `lists:map/2` call), never a wrong result — but never a right one either.
        let term = rt
            .eval(
                "begin T = ets:new(t, [set]), \
                 lists:map(fun(X) -> ets:insert(T, {X, X*X}) end, [1,2,3]), \
                 ets:lookup(T, 2) end",
            )
            .expect("ets:* inside a fun applied by lists:map/2");
        let printed = rt.format_term(term, None).expect("~p renders the lookup");
        assert_eq!(printed.trim_end(), "[{2,4}]");
    }

    #[test]
    fn a_nested_io_format_reaches_the_callers_sink() {
        let mut rt = runtime();
        let mut sink = BytesSink::new();
        {
            let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
            // The `io:format/2` runs inside the fun `lists:map/2` applies; it must write to
            // the caller's sink (shared through `SharedSink`), not to stdout.
            crate::eval::eval(
                &mut ctx,
                "lists:map(fun(X) -> io:format(\"~w \", [X]) end, [1,2,3])",
            )
            .expect("io:format inside a fun applied by lists:map/2");
        }
        assert_eq!(sink.as_str(), "1 2 3 ");
    }

    #[test]
    fn a_nested_call_without_a_fun_still_sees_the_outer_tables() {
        let mut rt = runtime();
        // The mirror case: a *non*-re-entrancy-capable BIF (`ets:new/2`, `ets:insert/2` here)
        // is given the shared tables, and a fun it does not apply cannot observe them.
        let term = rt
            .eval("begin T = ets:new(t, [set]), ets:insert(T, {1, 11}), ets:lookup(T, 1) end")
            .expect("a plain ets:* sequence");
        let printed = rt.format_term(term, None).expect("~p renders the lookup");
        assert_eq!(printed.trim_end(), "[{1,11}]");
    }

    #[test]
    fn the_arena_is_unbounded_in_milestone_one() {
        let mut rt = runtime();
        let mut ctx = RuntimeContext::new(&mut rt);
        assert_eq!(ctx.heap_available(), usize::MAX);
        let mut x = [make_small(0)];
        let mut stack = [make_small(0)];
        assert!(ctx.ensure_heap(u32::MAX, 1, &mut x, &mut stack).is_ok());
        assert!(!ctx.alloc_words(4).expect("arena allocates").is_null());
    }

    #[test]
    fn the_registry_is_the_core_bif_set() {
        let rt = runtime();
        let core = BifRegistry::with_core_bifs();
        for (module, function, arity) in [
            ("erlang", "+", 2),
            ("lists", "reverse", 1),
            ("io", "format", 2),
        ] {
            assert!(rt.registry.contains(module, function, arity));
            // Value-for-value agreement with a freshly built core registry proves this
            // is the real `beam_bifs` one, not a hand-rolled subset.
            assert_eq!(
                rt.registry.lookup(module, function, arity).is_some(),
                core.lookup(module, function, arity).is_some()
            );
        }
        assert!(!rt.registry.contains("erlang", "nope", 0));
        assert!(!core.contains("erlang", "nope", 0));
    }

    // -----------------------------------------------------------------------
    // the receive/mailbox seam over the running process's real mailbox
    // -----------------------------------------------------------------------

    /// The committed mailbox fixture (regeneration recipe in `tests/fixtures/README.md`).
    const MAILBOX_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/mailbox.beam"
    );

    /// The fixture loaded as an owned image, so it outlives the borrowing context
    /// (the receive-seam cases need `Machine::run_export_outcome`, which borrows both).
    fn mailbox_fixture() -> Module {
        beam_loader::load_file(MAILBOX_BEAM).expect("load mailbox.beam")
    }

    /// The fixture's pid term (the destination `self()` reports).
    fn self_term(rt: &Runtime) -> Eterm {
        rt.self_pid().to_eterm()
    }

    #[test]
    fn send_message_resolves_the_target_and_rejects_a_non_pid() {
        let mut rt = runtime();
        let pid = self_term(&rt);
        let mut ctx = RuntimeContext::new(&mut rt);

        // A self-delivery is the `Enqueued` case (nothing is blocked).
        ctx.send_message(pid, make_small(7)).expect("deliver to self");

        // A non-pid destination is the typed `BadArg` — never a wrong delivery.
        assert!(matches!(
            ctx.send_message(make_small(7), make_small(7)),
            Err(BeamError::BadArg)
        ));

        // The message is in the running process's mailbox.
        let mailbox = ctx
            .runtime
            .processes
            .get(ctx.runtime.self_pid)
            .expect("the runtime process")
            .mailbox();
        assert_eq!(mailbox.len(), 1);
        assert_eq!(mailbox.peek().expect("the delivered message").payload(), make_small(7));
    }

    #[test]
    fn send_message_to_an_unknown_pid_is_the_tables_typed_error() {
        let mut rt = runtime();
        let mut ctx = RuntimeContext::new(&mut rt);
        // A pid whose (number, serial) names no live process: the table's typed not-found.
        let ghost = ProcessId::new(9999, 7).to_eterm();
        let error = ctx
            .send_message(ghost, make_small(1))
            .expect_err("no such process");
        assert!(
            matches!(error, BeamError::TermNotFound),
            "expected the table's typed not-found error, got {error:?}"
        );
    }

    #[test]
    fn receive_next_peeks_and_the_scan_operations_position_the_mailbox() {
        let mut rt = runtime();
        let pid = self_term(&rt);
        let mut ctx = RuntimeContext::new(&mut rt);

        // An empty mailbox is `Ok(None)` (not an error) — that is what makes the
        // compiled code jump to the `wait` that blocks.
        assert_eq!(ctx.receive_next().expect("empty mailbox"), None);

        ctx.send_message(pid, make_small(1)).expect("first");
        ctx.send_message(pid, make_small(2)).expect("second");

        // `loop_rec` peeks: repeated calls yield the same candidate.
        assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
        assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));

        // `loop_rec_end` advances past it.
        ctx.receive_skip().unwrap();
        assert_eq!(ctx.receive_next().unwrap(), Some(make_small(2)));

        // `remove_message` unlinks the candidate and repositions at the first message.
        ctx.receive_remove().unwrap();
        assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));

        // `timeout` resets the scan (already first here).
        ctx.receive_reset().unwrap();
        assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
    }

    #[test]
    fn remove_message_unlinks_only_the_matched_candidate() {
        let mut rt = runtime();
        let pid = self_term(&rt);
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message(pid, make_small(1)).unwrap();
        ctx.send_message(pid, make_small(2)).unwrap();
        ctx.send_message(pid, make_small(3)).unwrap();

        ctx.receive_skip().unwrap(); // candidate `2`
        ctx.receive_remove().unwrap();

        let mailbox = ctx
            .runtime
            .processes
            .get(ctx.runtime.self_pid)
            .expect("the runtime process")
            .mailbox();
        let left: Vec<Eterm> = mailbox.iter().map(beam_message::Message::payload).collect();
        assert_eq!(left, vec![make_small(1), make_small(3)]);
        assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
    }

    #[test]
    fn an_empty_mailbox_blocks_and_the_plain_entry_is_the_typed_blocked_error() {
        // Mirrors the reference wait-on-an-empty-mailbox case: the engine reports `Wait`,
        // and the non-blocking entry folds that into the documented typed error.
        let image = mailbox_fixture();
        let wait = image.atom_table.lookup("wait").expect("wait/0");
        let mut rt = runtime();
        let mut machine = Machine::new();

        let outcome = {
            let mut ctx = RuntimeContext::new(&mut rt);
            machine
                .run_export_outcome(&image, wait, 0, &[], &mut ctx)
                .expect("wait/0")
        };
        assert!(
            matches!(outcome, RunOutcome::Wait { .. }),
            "an empty mailbox suspends the receive, got {outcome:?}"
        );

        // The non-blocking entry folds the suspension into the documented typed error.
        let error = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.run_export(&image, "wait", 0, &[])
                .expect_err("run_export is non-blocking")
        };
        assert_eq!(error.to_string(), beam_engine::blocked().to_string());
    }

    #[test]
    fn a_suspended_receive_resumes_against_the_runtime_stored_scan_position() {
        // The point of storing the scan state per pid: the resume happens through a
        // **fresh context**, and it still finds the position where the receive blocked.
        let image = mailbox_fixture();
        let wait = image.atom_table.lookup("wait").expect("wait/0");
        let mut rt = runtime();
        let mut machine = Machine::new();

        let outcome = {
            let mut ctx = RuntimeContext::new(&mut rt);
            machine
                .run_export_outcome(&image, wait, 0, &[], &mut ctx)
                .expect("wait/0")
        };
        let (func, pc) = match outcome {
            RunOutcome::Wait { func, pc } => (func, pc),
            other => panic!("expected a wait, got {other:?}"),
        };

        // Deliver a message to the runtime's own mailbox (a `receive` waiting on it).
        let pid = rt.self_pid();
        rt.processes
            .deliver_simple(pid, pid.to_eterm(), make_small(42))
            .expect("deliver");

        let mut ctx = RuntimeContext::new(&mut rt);
        let resumed = machine
            .resume_outcome(&image, func, pc, &mut ctx)
            .expect("resume at the wait target");
        assert_eq!(resumed, RunOutcome::Returned(make_small(42)));
        let mailbox = ctx
            .runtime
            .processes
            .get(ctx.runtime.self_pid)
            .expect("the runtime process")
            .mailbox();
        assert!(
            mailbox.is_empty(),
            "the resumed receive removed the matched message"
        );
    }

    #[test]
    fn scan_skips_a_non_matching_head_and_keeps_it_queued() {
        // Mirrors the reference `scan_skips_a_non_matching_head_before_matching`.
        let image = mailbox_fixture();
        let mut rt = runtime();
        let result = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.run_export(&image, "scan", 0, &[]).expect("scan/0")
        };
        assert_eq!(result, make_small(7), "the second message matched");

        let mailbox = rt
            .processes
            .get(rt.self_pid())
            .expect("the runtime process")
            .mailbox();
        assert_eq!(mailbox.len(), 1, "only the matched message was removed");
        assert_eq!(
            mailbox.peek().expect("the scanned-past head").payload(),
            make_small(42)
        );
    }

    #[test]
    fn a_marker_reserved_on_an_empty_queue_is_the_shared_undefined_atom() {
        let mut rt = runtime();
        // The atom `receive_marker_reserve` interns is the C `am_undefined`.
        let undefined = rt.intern("undefined");
        let mut ctx = RuntimeContext::new(&mut rt);

        let marker = ctx.receive_marker_reserve().expect("reserve");
        assert_eq!(marker, undefined, "an empty queue has no position to record");
        assert!(!is_small(marker), "`undefined` is an atom, not a marker id");

        // Binding it is a silent no-op, and using the reference cannot move the scan.
        ctx.receive_marker_bind(marker, make_small(1)).unwrap();
        ctx.receive_marker_use(make_small(1)).unwrap();
        assert_eq!(ctx.receive_next().unwrap(), None);
    }

    #[test]
    fn the_marker_prologue_repositions_the_scan() {
        // Mirrors the reference `the_mailbox_repositions_the_scan_at_a_marker`, driven
        // through the real context (so it exercises `RuntimeContext::mailbox_parts`).
        let mut rt = runtime();
        let pid = self_term(&rt);
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message(pid, make_small(1)).unwrap();

        ctx.receive_skip().unwrap(); // scan past `1`
        let marker = ctx.receive_marker_reserve().expect("reserve");
        assert!(is_small(marker), "a recorded marker is a small integer");
        ctx.receive_marker_bind(marker, make_small(999)).unwrap();

        // A later message (self-delivered through the same context).
        ctx.send_message(pid, make_small(2)).unwrap();

        ctx.receive_reset().unwrap();
        assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
        ctx.receive_marker_use(make_small(999)).unwrap();
        assert_eq!(
            ctx.receive_next().unwrap(),
            Some(make_small(2)),
            "the marker moved the scan past the first message"
        );

        ctx.receive_marker_clear(make_small(999)).unwrap();
        ctx.receive_reset().unwrap();
        ctx.receive_marker_use(make_small(999)).unwrap();
        assert_eq!(
            ctx.receive_next().unwrap(),
            Some(make_small(1)),
            "a cleared marker no longer repositions the scan"
        );
    }

    #[test]
    fn for_process_resolves_the_mailbox_against_the_supplied_table() {
        // The scheduler-pass seam: a context running a pid in a table it owns separately
        // from the runtime, with the scan state still keyed by pid in the runtime.
        let mut rt = runtime();
        let mut table = beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::prelude::AtomTable::new(),
        ));
        let pid = table.spawn_default().expect("spawn a process");
        let undefined = rt.intern("undefined");

        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        assert_eq!(ctx.pid(), pid);
        // Empty: `Ok(None)` from the external table's process, `undefined` marker.
        assert_eq!(ctx.receive_next().unwrap(), None);
        assert_eq!(ctx.receive_marker_reserve().unwrap(), undefined);

        // A self-delivery lands in the external table's process mailbox.
        ctx.send_message(pid.to_eterm(), make_small(5)).unwrap();
        assert_eq!(ctx.receive_next().unwrap(), Some(make_small(5)));

        // `erlang:self/0` reports the context's pid, not the runtime's own.
        let self_term = ctx.call_bif("erlang", "self", 0, &[]).expect("self/0");
        assert_eq!(self_term, pid.to_eterm());

        // Drop the context to release its borrow of `table`, then inspect the mailbox.
        drop(ctx);
        assert_eq!(table.get(pid).expect("the process").mailbox().len(), 1);
    }

    #[test]
    fn for_process_reports_a_missing_process_as_the_typed_capability_gap() {
        let mut rt = runtime();
        let mut table = beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::prelude::AtomTable::new(),
        ));
        let pid = table.spawn_default().expect("spawn a process");

        // Remove the process *before* building the context: a receive-seam call must be
        // the typed `unsupported` capability gap, never a panic and never a wrong result.
        table.take(pid).expect("take the process");
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        let error = ctx.receive_next().expect_err("no mailbox for the pid");
        assert_eq!(
            crate::error::unsupported_reason(&error).as_deref(),
            Some(format!("receive: process {pid:?} has no mailbox").as_str())
        );
    }
}
