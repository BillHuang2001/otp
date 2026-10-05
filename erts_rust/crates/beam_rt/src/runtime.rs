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

use std::path::{Path, PathBuf};

use beam_alloc::Heap;
use beam_bifs::{BifRegistry, OutputSink, TableRegistry};
use beam_common::Result;
use beam_engine::{terms, ExecutionContext, MinimalContext};
use beam_loader::Module;
use beam_process::{ProcessId, ProcessTable, SignalAtoms};
use beam_term::{atom_val, is_atom, make_atom, make_small, AtomTable, Eterm};

use crate::boot::{self, PreloadedModules};
use crate::codeserver::CodeServer;
use crate::config::{ArgTerm, InitArgs, RuntimeConfig};
use crate::context::RuntimeContext;
use crate::error;
use crate::format;
use crate::info::{self, InfoValue};

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
    pub(crate) registry: BifRegistry,
    /// The code server: the one shared atom table, the module cache, the code path.
    pub(crate) code_server: CodeServer,
    /// The process table (Milestone-1: the runtime's own single process).
    processes: ProcessTable,
    /// The preloaded-module registry skeleton (contract A.7).
    preloaded: PreloadedModules,
    /// The process heap executed code allocates on; owned so results stay valid.
    pub(crate) heap: Heap,
    /// The pointer-stable arena serving the engine's `alloc_words` seam.
    pub(crate) arena: MinimalContext,
    /// The scratch heap for `system_info`/`format_term`, reset per call.
    pub(crate) info_heap: Heap,
    /// The `ets:*` tables.
    pub(crate) tables: TableRegistry,
    /// The pid code runs as (`erlang:self/0`).
    pub(crate) self_pid: ProcessId,
    /// Whether [`Runtime::boot`] has run.
    booted: bool,
    /// The exit status recorded by a native `erlang:halt/1` (see [`crate::native`]).
    pub(crate) halt: Option<i32>,
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
        // A zero-word heap can hold nothing, so the configured size is floored: the
        // runtime must always be able to run the smallest program.
        let heap_words = config.heap_words.max(MIN_HEAP_WORDS);
        Ok(Runtime {
            config,
            registry: BifRegistry::with_core_bifs(),
            code_server,
            processes,
            preloaded: PreloadedModules::standard(),
            heap: Heap::new(heap_words)?,
            arena: MinimalContext::new(),
            info_heap: Heap::new(INFO_HEAP_WORDS)?,
            tables: TableRegistry::new(),
            self_pid,
            booted: false,
            halt: None,
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

    /// Allocates `n` raw heap words from the pointer-stable arena.
    ///
    /// # Errors
    ///
    /// Propagates the arena's allocation failure.
    pub fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
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

    /// Materializes parsed command-line arguments into terms on the shared atom table
    /// and heap (an atom's index can only be assigned here).
    ///
    /// # Errors
    ///
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
