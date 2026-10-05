//! The runtime façade: boot, the module cache and the **vertical slice**.
//!
//! [`Runtime`] is the whole picture in one object. It owns
//!
//! * the parsed [`RuntimeConfig`] and derived state (code path, heap size);
//! * the BIF [`BifRegistry`] (`beam_bifs`'s `bif.tab` surface) and a
//!   [`TableRegistry`] (`ets:*`);
//! * the **global** [`AtomTable`] — used only by `system_info/1` and the `~p` printer;
//! * the [`ProcessTable`] and the runtime's own [`ProcessId`];
//! * the preloaded-module registry skeleton (contract A.7);
//! * the process [`Heap`] (owned, so a term returned by `run_*` stays valid) and a
//!   separate scratch `info_heap` for `system_info`/`format_term`;
//! * the [`Module`] cache, keyed by module name (each cached module keeps its own
//!   atom table, which is what the atom-table bridge needs).
//!
//! `Runtime::run_function`/`run_module` are the evidence for this crate: they load a
//! real `.beam` and execute it through the real [`RuntimeContext`] and the real BIF
//! registry — the same path the `beam_rt` executable takes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use beam_alloc::Heap;
use beam_bifs::{BifRegistry, OutputSink, StdoutSink, TableRegistry};
use beam_common::Result;
use beam_engine::Machine;
use beam_loader::{load_file, Module};
use beam_process::{ProcessId, ProcessTable, SignalAtoms};
use beam_term::{AtomTable, Eterm};

use crate::boot::{self, PreloadedModules};
use crate::config::RuntimeConfig;
use crate::context::RuntimeContext;
use crate::error;
use crate::eval;
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
    registry: BifRegistry,
    /// The **global** atom table (`system_info` and the printer; see the module docs).
    atoms: AtomTable,
    /// The process table (Milestone-1: the runtime's own single process).
    processes: ProcessTable,
    /// The preloaded-module registry skeleton (contract A.7).
    preloaded: PreloadedModules,
    /// The process heap executed code allocates on; owned so results stay valid.
    heap: Heap,
    /// The scratch heap for `system_info`/`format_term`, reset per call.
    info_heap: Heap,
    /// The `ets:*` tables.
    tables: TableRegistry,
    /// The pid code runs as (`erlang:self/0`).
    self_pid: ProcessId,
    /// The loaded-module cache, keyed by module name.
    modules: HashMap<String, Module>,
    /// Whether [`Runtime::boot`] has run.
    booted: bool,
}

impl Runtime {
    /// Creates a runtime for `config`.
    ///
    /// The registries, the atom table, the process table and the heaps are set up
    /// immediately; the A.7 **boot step** runs on demand (or explicitly through
    /// [`Runtime::boot`]).
    ///
    /// # Errors
    ///
    /// Propagates heap/process allocation failures and an out-of-range `+P`-style
    /// limit.
    pub fn new(config: RuntimeConfig) -> Result<Runtime> {
        let mut atoms = AtomTable::new();
        boot::seed_core_atoms(&mut atoms);
        let signals = SignalAtoms::from_table(&mut atoms);
        let mut processes = ProcessTable::new(signals);
        let self_pid = processes.spawn_default()?;
        // A zero-word heap can hold nothing, so the configured size is floored: the
        // runtime must always be able to run the smallest program.
        let heap_words = config.heap_words.max(MIN_HEAP_WORDS);
        Ok(Runtime {
            config,
            registry: BifRegistry::with_core_bifs(),
            atoms,
            processes,
            preloaded: PreloadedModules::standard(),
            heap: Heap::new(heap_words)?,
            info_heap: Heap::new(INFO_HEAP_WORDS)?,
            tables: TableRegistry::new(),
            self_pid,
            modules: HashMap::new(),
            booted: false,
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
        beam_common::beam_info!(
            "beam_rt {} booted: {} preloaded modules, {} core atoms, 1 process",
            beam_common::ERTS_VSN,
            self.preloaded.len(),
            self.atoms.len(),
        );
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

    /// The preloaded-module registry skeleton (contract A.7).
    pub fn preloaded(&self) -> &PreloadedModules {
        &self.preloaded
    }

    /// The process table (Milestone-1: one process — the runtime itself).
    pub(crate) fn processes(&self) -> &ProcessTable {
        &self.processes
    }

    /// The pid the runtime runs as.
    pub fn self_pid(&self) -> ProcessId {
        self.self_pid
    }

    /// Loads `path` and caches the module under its own name.
    ///
    /// A module loaded twice replaces the cached one (mirroring a code reload).
    ///
    /// # Errors
    ///
    /// Propagates the loader's error (a malformed container, an unsupported chunk, …)
    /// or returns a diagnostic when the module has no resolvable name.
    pub fn load_module(&mut self, path: impl AsRef<Path>) -> Result<&Module> {
        let module = load_file(path)?;
        let name = module
            .atom_name(module.name)
            .ok_or_else(|| error::unknown_module("<unnamed .beam>"))?
            .to_string();
        self.modules.insert(name.clone(), module);
        self.modules
            .get(&name)
            .ok_or_else(|| error::unknown_module(&name))
    }

    /// A cached module by name, if it has been loaded.
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.modules.get(name)
    }

    /// The names of the cached modules, sorted.
    pub fn loaded_modules(&self) -> Vec<String> {
        let mut names: Vec<String> = self.modules.keys().cloned().collect();
        names.sort();
        names
    }

    /// Finds `<module>.beam` in the configured code path (`-pa`, then `-pz`).
    pub(crate) fn find_module_file(&self, module: &str) -> Option<PathBuf> {
        let file = format!("{module}.beam");
        self.config
            .code_paths
            .iter()
            .map(|dir| dir.join(&file))
            .find(|candidate| candidate.is_file())
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
        let name = {
            let module = self.load_module(path)?;
            module
                .atom_name(module.name)
                .ok_or_else(|| error::unknown_module("<unnamed .beam>"))?
                .to_string()
        };
        let mut sink = StdoutSink;
        let Runtime {
            registry,
            heap,
            tables,
            self_pid,
            modules,
            ..
        } = self;
        let module = modules
            .get(&name)
            .ok_or_else(|| error::unknown_module(&name))?;
        execute(
            module, function, arity, args, registry, heap, tables, *self_pid, &mut sink,
        )
    }

    /// The general primitive: run `function/arity` against a **caller-owned** module
    /// with an explicit output sink.
    ///
    /// This is what `-s Mod Func Args…` uses (the caller loads the `.beam` itself, so
    /// it can keep the `Module` — and its atom table — alive while resolving the atom
    /// arguments).
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
        let Runtime {
            registry,
            heap,
            tables,
            self_pid,
            ..
        } = self;
        execute(
            module, function, arity, args, registry, heap, tables, *self_pid, out,
        )
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
        value.to_eterm(&mut self.info_heap, &self.atoms)
    }

    /// `system_info/1` as a heap-independent value — the testable source of truth.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an unknown key.
    pub fn system_info_value(&self, key: &str) -> Result<InfoValue> {
        info::system_info_value(key, self)
    }

    /// Evaluates the documented `-eval` integer subset (contract A.6).
    ///
    /// The result is built on the process heap, so it stays valid for the runtime's
    /// lifetime.
    ///
    /// # Errors
    ///
    /// A diagnostic for an expression outside the subset, plus the arithmetic errors of
    /// the dispatched `erlang` BIFs.
    pub fn eval(&mut self, expr: &str) -> Result<Eterm> {
        let Runtime {
            registry,
            heap,
            atoms,
            self_pid,
            ..
        } = self;
        eval::eval(expr, registry, heap, atoms, *self_pid)
    }

    /// Prints `term` through the registered `io:format/2` `~p` printer.
    ///
    /// `module` selects the atom table the printer resolves atoms against: the loaded
    /// module's (the one that produced the term) when given, the runtime's global table
    /// otherwise. Like [`Runtime::system_info`], the format/argument lists are built on
    /// the scratch heap, so the returned text — not any term — is the result.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a term outside the
    /// Milestone-1 printable set; the heap's allocation failure otherwise.
    pub fn format_term(&mut self, term: Eterm, module: Option<&Module>) -> Result<String> {
        self.info_heap.reset();
        let Runtime {
            info_heap,
            atoms,
            registry,
            self_pid,
            ..
        } = self;
        let table = match module {
            Some(module) => &module.atom_table,
            None => &*atoms,
        };
        format::term_to_string(registry, info_heap, table, *self_pid, term)
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

/// Runs `function/arity` of `module` through the real context.
///
/// The atom-table bridge lives here: the [`RuntimeContext`] gets the **module's** atom
/// table, so the atoms BIFs build are the indices the engine compares against.
fn execute(
    module: &Module,
    function: &str,
    arity: u32,
    args: &[Eterm],
    registry: &BifRegistry,
    heap: &mut Heap,
    tables: &mut TableRegistry,
    self_pid: ProcessId,
    out: &mut dyn OutputSink,
) -> Result<Eterm> {
    let module_name = module.atom_name(module.name).unwrap_or("<unnamed>");
    let function_atom = module
        .atom_table
        .lookup(function)
        .ok_or_else(|| error::unknown_function(module_name, function, arity))?;
    if module.export(function_atom, arity).is_none() {
        return Err(error::unknown_function(module_name, function, arity));
    }
    let mut context =
        RuntimeContext::new(registry, heap, &module.atom_table, self_pid, tables).with_output(out);
    Machine::new().run_export(module, function_atom, arity, args, &mut context)
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
        assert!(rt.atoms.lookup("unix").is_some());
        assert!(rt.atoms.lookup("linux").is_some());
        assert_eq!(rt.processes.len(), 1);
        assert_eq!(
            rt.system_info_value("process_count").expect("count"),
            InfoValue::Int(1)
        );
    }

    #[test]
    fn loads_and_caches_a_module_with_its_atom_table() {
        let mut rt = runtime();
        let module = rt.load_module(SLICE_BEAM).expect("load slice.beam");
        assert_eq!(module.atom_name(module.name), Some("slice"));
        assert_eq!(rt.loaded_modules(), vec!["slice".to_string()]);
        assert!(rt.module("slice").is_some());
        assert!(rt.module("nope").is_none());
        // The cached module keeps its own atom table for the engine to compare against.
        assert!(rt
            .module("slice")
            .expect("cached")
            .atom_table
            .lookup("pick")
            .is_some());
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
    fn the_slice_takes_an_atom_argument_from_the_module_table() {
        // `run_module` takes a **caller-owned** module, exactly as the CLI's `-s`
        // action does (it must keep the `Module` — and its atom table — alive while
        // resolving the atom arguments).
        let image = load_file(SLICE_BEAM).expect("load slice.beam");
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
        // A `system_info` term lives on the scratch heap, which the *next*
        // `system_info`/`format_term` call resets — so it is read, never returned.
        let version = rt.system_info("version").expect("version");
        assert!(beam_term::is_list(version));
        let again = rt.system_info("wordsize").expect("wordsize again");
        assert_eq!(signed_value(again), std::mem::size_of::<usize>() as i64);
        // Terms on the process heap (`eval` results, `run_*` results) are stable and
        // print through the registered `~p` printer.
        let five = rt.eval("2+3").expect("2+3");
        assert_eq!(rt.format_term(five, None).expect("printable"), "5\n");
    }

    #[test]
    fn eval_dispatches_through_the_bif_registry() {
        let mut rt = runtime();
        assert_eq!(signed_value(rt.eval("2+3").expect("2+3")), 5);
        assert_eq!(signed_value(rt.eval("2*3+4").expect("2*3+4")), 10);
        assert_eq!(signed_value(rt.eval("(1+2)*3").expect("(1+2)*3")), 9);
        assert!(rt.eval("nope").is_err());
    }

    #[test]
    fn format_term_uses_the_registered_printer() {
        let mut rt = runtime();
        assert_eq!(
            rt.format_term(make_small(7), None).expect("printable"),
            "7\n"
        );
        assert_eq!(rt.format_term(beam_term::NIL, None).expect("printable"), "[]\n");
    }

    #[test]
    fn module_files_are_found_through_the_code_path() {
        let dir = Path::new(SLICE_BEAM).parent().expect("a parent directory");
        let config = RuntimeConfig::default().with_code_path(dir);
        let rt = Runtime::new(config).expect("runtime");
        assert_eq!(rt.find_module_file("slice"), Some(PathBuf::from(SLICE_BEAM)));
        assert_eq!(rt.find_module_file("nope"), None);
    }
}
