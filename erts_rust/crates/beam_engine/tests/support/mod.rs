#![allow(dead_code)]
//! A **multi-module** test [`ExecutionContext`] for the stdlib integration tests.
//!
//! Where `tests/coverage.rs`'s `CoverageCtx` serves a *single* hand-authored
//! fixture with a fixed BIF hook, this host runs **real system stdlib modules**:
//! it loads a `.beam` by name (either an explicitly registered fixture or a
//! module from the running Erlang's stdlib directory), runs an export on a fresh
//! [`Machine`] that shares this one context, and resolves a `.beam`'s anonymous
//! funs back to their defining module so `call_fun`/`call_fun2` work across
//! modules.
//!
//! # The two global resources a real BEAM has — and this host must emulate
//!
//! 1. **A single term heap.** Every term built by any module comes from this one
//!    context's [`MinimalContext`] arena, so a term produced by `lists` is
//!    readable by the harness and vice versa.
//! 2. **A single atom table.** A real runtime interns atoms globally; here
//!    [`beam_loader::load_file`] would build a *private* [`AtomTable`] per module
//!    (indices assigned in wire order), so the same name (`true`, `ok`, …) would
//!    get different indices in different modules — and `beam_term::cmp` resolves
//!    atom *names* through the running module's table, so a cross-module atom
//!    comparison (e.g. `lists:filter` matching a predicate's `true`/`false`
//!    result) would silently fail. This host therefore re-runs the loader's own
//!    public pipeline ([`container`](beam_loader::container) →
//!    [`chunk`](beam_loader::chunk) → [`decode`](beam_loader::decode) →
//!    [`lower::build_module`](beam_loader::lower::build_module)) seeded with **one
//!    shared [`AtomTable`]**, exactly reproducing `load_file` but with a global
//!    atom universe. Interning is idempotent and name-keyed, so every module sees
//!    the same index for a given name.
//!
//! # Dispatch routing
//!
//! The engine routes `call_ext*` **and** `apply`/`apply_last` through
//! [`ExecutionContext::call_bif`], and only `call_fun`/`call_fun2` through
//! [`ExecutionContext::apply_fun`]. So [`ModuleHost::call_bif`] serves both the
//! built-in BIF floor *and* external module calls, and [`ModuleHost::apply_fun`]
//! resolves a closure to its defining `.beam`.
//!
//! Ground truth for every asserted value is the real system Erlang (`erl`); the
//! tests never guess (see `tests/fixtures/README.md`).

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{Atom, FunDescriptor, Module};
use beam_term::{
    atom_val, cmp, cmp_arith, is_atom, is_small, make_atom, make_small, signed_value, AtomTable,
    Eterm, ERL_ONHEAP_BINARY_LIMIT, MAX_ARITYVAL, MAX_SMALL, MIN_SMALL,
};

// ---------------------------------------------------------------------------
// Locating the system stdlib (with an env override + a loud skip on failure)
// ---------------------------------------------------------------------------

/// The `ebin` directory of the running Erlang's `stdlib` application, or `None`
/// when neither `erl` nor a usable stdlib directory can be found.
///
/// Resolution order:
/// 1. `$ERTS_STDLIB_DIR` (accepts either the stdlib *application* dir —
///    `…/lib/stdlib-8.1` — or its `ebin` subdir);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir(stdlib)]),halt().'`
///    evaluated at test time.
///
/// A caller that gets `None` must **skip loudly** (never fail): see
/// [`skip_reason`].
pub fn stdlib_ebin() -> Option<PathBuf> {
    app_ebin("stdlib")
}

/// The `ebin` directory of the running Erlang's `app` application (e.g.
/// `stdlib`, `kernel`, `compiler`), or `None` when it cannot be located.
///
/// Resolution order:
/// 1. `$ERTS_<APP>_DIR` (uppercased app name; accepts either the application dir
///    — `…/lib/stdlib-8.1` — or its `ebin` subdir);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir(<app>)]),halt().'`
///    evaluated at test time.
///
/// This is the general form of [`stdlib_ebin`]; a caller that gets `None` must
/// **skip loudly** (never fail) — see [`skip_reason`].
pub fn app_ebin(app: &str) -> Option<PathBuf> {
    let env_key = format!("ERTS_{}_DIR", app.to_uppercase());
    if let Ok(dir) = env::var(&env_key) {
        let dir = PathBuf::from(dir);
        if has_beams(&dir) {
            return Some(dir);
        }
        let ebin = dir.join("ebin");
        if has_beams(&ebin) {
            return Some(ebin);
        }
        return None;
    }

    let eval = format!("io:format(\"~s\",[code:lib_dir({app})]),halt().");
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8(output.stdout).ok()?;
    let ebin = PathBuf::from(dir.trim()).join("ebin");
    if has_beams(&ebin) {
        Some(ebin)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Locating the WHOLE installed OTP (every application, generically)
// ---------------------------------------------------------------------------

/// The root `lib` directory of the running Erlang installation — the directory
/// that holds the versioned application directories (`stdlib-8.1`,
/// `kernel-11.0.4`, …), i.e. exactly what `code:lib_dir/0` returns.
///
/// Resolution order:
/// 1. `$ERTS_LIB_DIR` (the lib *root* itself);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir()]),halt().'`.
///
/// `None` when neither yields an existing directory; a caller that gets `None`
/// must **skip loudly** (never fail) — see [`skip_reason`].
pub fn otp_lib_root() -> Option<PathBuf> {
    if let Ok(dir) = env::var("ERTS_LIB_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
    }
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir()]),halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    dir.is_dir().then_some(dir)
}

/// One installed application's `ebin` directory together with the sorted
/// basenames of the `*.beam` modules it holds.
pub struct AppEbin {
    /// The application *directory* name as installed, e.g. `stdlib-8.1` (or the
    /// bare name when the directory carries no version suffix).
    pub app: String,
    /// The `<app>/ebin` directory holding the modules.
    pub dir: PathBuf,
    /// The sorted `*.beam` module basenames (without the extension).
    pub modules: Vec<String>,
}

/// Every installed OTP application's `ebin` directory — with its sorted `.beam`
/// module basenames — discovered **generically** under [`otp_lib_root`]: each
/// direct child directory of the lib root that contains an `ebin` with at least
/// one `.beam` becomes one entry. **No application name or path is hardcoded**,
/// so an OTP install that gains/renames/removes an application is walked as-is.
///
/// Entries are returned sorted by application directory name, so the order is
/// stable across runs. A directory without a readable `ebin` (or with no
/// `.beam` at all, e.g. `erl_interface`) is skipped. `None` means the lib root
/// itself could not be located (the caller skips loudly).
pub fn all_otp_ebins() -> Option<Vec<AppEbin>> {
    let root = otp_lib_root()?;
    let mut app_dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    app_dirs.sort();

    let mut apps: Vec<AppEbin> = Vec::new();
    for app_dir in app_dirs {
        let ebin = app_dir.join("ebin");
        let mut modules: Vec<String> = std::fs::read_dir(&ebin)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|ext| ext == "beam").unwrap_or(false))
            .filter_map(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
            .collect();
        if modules.is_empty() {
            continue;
        }
        modules.sort();
        let app = app_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        apps.push(AppEbin {
            app,
            dir: ebin,
            modules,
        });
    }
    Some(apps)
}

/// One `.beam` file anywhere under the installed OTP lib root.
pub struct BeamFile {
    /// The owning application *directory* name as installed (e.g. `stdlib-8.1`).
    pub app: String,
    /// The `.beam` file stem (the module name the loader will see).
    pub module: String,
    /// The full path of the `.beam` file.
    pub path: PathBuf,
}

/// **Every** `*.beam` file anywhere under the installed OTP lib root, discovered
/// generically by walking each application directory recursively — the `ebin`
/// modules as well as any compiled `.beam` a distribution ships elsewhere under
/// the app (documentation examples, `proper_ext`, …). No application name, path
/// or module is hardcoded.
///
/// Entries are returned sorted by `(app, module)` so the order is stable across
/// runs. `None` means the lib root itself could not be located (the caller skips
/// loudly).
pub fn all_otp_beams() -> Option<Vec<BeamFile>> {
    let root = otp_lib_root()?;
    let mut beams: Vec<BeamFile> = Vec::new();
    for entry in std::fs::read_dir(&root).ok()?.filter_map(|entry| entry.ok()) {
        let app_dir = entry.path();
        if !app_dir.is_dir() {
            continue;
        }
        let app = app_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // An iterative depth-first walk (no recursion bound, no external crate).
        let mut stack = vec![app_dir];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(|entry| entry.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().map(|ext| ext == "beam").unwrap_or(false) {
                    let module = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    beams.push(BeamFile {
                        app: app.clone(),
                        module,
                        path,
                    });
                }
            }
        }
    }
    beams.sort_by(|a, b| a.app.cmp(&b.app).then_with(|| a.module.cmp(&b.module)));
    Some(beams)
}

/// True when `dir` is a directory that contains at least one `*.beam` file — the
/// signal [`app_ebin`] uses to accept a candidate `ebin` directory.
fn has_beams(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .path()
            .extension()
            .map(|extension| extension == "beam")
            .unwrap_or(false)
    })
}

/// The message a test prints (and the reason it returns early) when the oracle
/// stdlib is unavailable. `None` means "available — do not skip".
pub fn skip_reason() -> Option<String> {
    if stdlib_ebin().is_none() {
        Some(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the stdlib integration tests require the OTP stdlib `.beam` files as the oracle"
                .to_string(),
        )
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Loading with one shared atom table
// ---------------------------------------------------------------------------

/// The runtime's **well-known atoms** — the C `error_atom[NUMBER_EXIT_CODES]`
/// table (`../../erts/emulator/beam/beam_common.c:382-406`) plus the
/// exception-*class* and auxiliary atoms the exceptions family resolves by name
/// (`src/families/exceptions/`: `install_class_atom`, `catch_end`'s `'EXIT'`,
/// `build_stacktrace`'s `file`/`line`).
///
/// A real runtime keeps these **globally interned**, so they are nameable from
/// *every* module whether or not the module's own source ever mentions them. The
/// real OTP compiler does **not** emit them into a module's atom chunk — e.g.
/// `erlc p.erl` on a module whose functions raise `function_clause` /
/// `{case_clause,_}` / `if_clause` / `{badmatch,_}` / `{try_clause,_}` still
/// produces an atom list containing none of them — so a module loaded on its own
/// cannot name its own exception reasons. Seeding them into every loaded module's
/// atom table is this host's analogue of the runtime's global interning; the
/// production counterpart is `beam_rt::boot::CORE_ATOMS`.
///
/// The engine's lookup policy is deliberately **unchanged**: it still resolves a
/// reason atom by name from the running module's table and reports a typed
/// `unsupported` when the name is absent, so a *different* atom is never silently
/// substituted.
///
/// Exposed (`pub`) so a test can assert the exact seed set that the host interns.
pub const SEED_ATOMS: &[&str] = &[
    // The C `error_atom` table, in file order (`beam_common.c:382-406`).
    "internal_error",
    "normal",
    "badarg",
    "badarith",
    "badmatch",
    "function_clause",
    "case_clause",
    "if_clause",
    "undef",
    "badfun",
    "badarity",
    "timeout_value",
    "noproc",
    "notalive",
    "system_limit",
    "try_clause",
    "notsup",
    "badmap",
    "badkey",
    "badrecord",
    "badfield",
    "novalue",
    // The exception *class* atoms a landing installs into `x(3)` (`error`/`exit`/
    // `throw`), plus the auxiliaries the family looks up by name: `'EXIT'`
    // (`catch_end`'s `{'EXIT',Reason}`), `file`/`line` (`build_stacktrace`'s
    // source location).
    "error",
    "exit",
    "throw",
    "EXIT",
    "file",
    "line",
];

/// Loads a `.beam` image into the IR, interning its atoms (and its decoded
/// literal atoms) into `atoms` — the host's one shared [`AtomTable`].
///
/// This is [`beam_loader::load`]'s exact pipeline, re-run through the loader's
/// public modules so the shared table (not a private per-module one) is the
/// interning context. See the module docs for why that matters.
///
/// # The index-consistency invariant
///
/// After this returns, **for every index `i` in the module's atom table,
/// `module.atom_table.name(i) == atoms.name(i)`** — the shared table and every
/// loaded module agree on `Atom -> name` for all of the module's indices. That is
/// what makes a term built by one module compare equal to the same-named atom
/// interned by another (and lets the tests resolve names through
/// `ModuleHost::atom_table`). The invariant is maintained by APPENDING to the
/// shared table only:
/// * the wire loop pre-interns the chunk atoms into `atoms`, so `build_module`'s
///   clone already holds them at the same indices;
/// * `build_module` can only *append* — a literal-only atom is new to the clone,
///   so it lands at `atoms.len()..`, and re-interning it here in index order
///   appends it to `atoms` at exactly the same index;
/// * the [`SEED_ATOMS`] are then appended to both tables in lockstep (each name is
///   interned into `atoms` first and into the module's own table second; both
///   tables are equal-length and consistent before each iteration, so both return
///   the same index whether the name is new or already present).
///
/// Existing indices are therefore never shifted, so atom-keyed structures built by
/// earlier modules (e.g. flat-map storage order) are untouched.
fn load_with_shared_atoms(bytes: &[u8], atoms: &mut AtomTable) -> Result<Module> {
    use beam_loader::{chunk, container, decode, lower};

    let container = container::parse(bytes)?;
    let chunks = chunk::parse(&container)?;

    // `atoms[0]` is the NIL placeholder; `atoms[1]` is the module name (mirrors
    // `load`). Interning is idempotent, so a name shared with an earlier module
    // keeps that module's index.
    let mut wire: Vec<Atom> = Vec::with_capacity(chunks.atoms.len());
    wire.push(Atom::MAX);
    for name in &chunks.atoms[1..] {
        wire.push(atoms.intern(name));
    }

    let raw = decode::decode_stream(
        chunks.code_stream,
        &wire,
        &chunks.code_version,
        &chunks.types,
    )?;

    // `build_module` owns (and may extend) a clone of the shared table while it
    // decodes the `LitT` literals; any atom that appeared only in a literal is
    // appended after the clone's prefix at exactly `atoms.len()..`, so merging
    // them back in index order keeps the shared indices identical.
    let mut module = lower::build_module(&chunks, &raw, atoms.clone())?;
    let base = atoms.len();
    for index in base..module.atom_table.len() {
        if let Some(name) = module.atom_table.name(Atom::from_raw(index as u32)) {
            atoms.intern(name);
        }
    }

    // Append the runtime's well-known atoms to the module's own table, in lockstep
    // with the shared table so the index-consistency invariant holds (see above).
    // `atoms` is interned first; because the two tables are equal-length and
    // consistent for all their indices, the module's table resolves the same name
    // to the same index — whether it is already present or newly appended.
    for &name in SEED_ATOMS {
        let shared = atoms.intern(name);
        let local = module.atom_table.intern(name);
        debug_assert_eq!(
            shared, local,
            "seed atom `{name}` broke the module/shared atom-table index consistency"
        );
    }

    Ok(module)
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// A multi-module execution context over one shared heap and one shared atom
/// table.
pub struct ModuleHost {
    /// The pointer-stable `alloc_words` arena (shared by every module).
    arena: MinimalContext,
    /// The one shared atom table every loaded module is interned into.
    atoms: AtomTable,
    /// Loaded modules, by name.
    modules: HashMap<String, Arc<Module>>,
    /// Explicit `name -> .beam path` registrations (the harness fixture).
    explicit: HashMap<String, PathBuf>,
    /// The stdlib `ebin` directory used as the primary code path.
    ebin: PathBuf,
    /// Extra `ebin` directories searched after `ebin` (e.g. `kernel`'s `ebin`),
    /// so a cross-module call into a second application resolves.
    search: Vec<PathBuf>,
    /// Every [`Machine`] this host has run, kept alive for the host's lifetime.
    ///
    /// A `Machine` owns the **off-heap ref-counted binary payloads** its runs built
    /// ([`Machine::alloc_refc_binary`]): a term bigger than 64 bytes — e.g.
    /// `base64:encode/1`'s 80-byte result — is an on-heap `[BinRef][ErlSubBits]` pair
    /// pointing at a machine-owned payload. The host consumes a returned `Eterm`
    /// *after* the run, so dropping the machine that produced it would free a payload
    /// still in use. Keeping every machine alive is the test-host analogue of a real
    /// process heap, which outlives a function return.
    machines: Vec<Machine>,}

impl ModuleHost {
    /// A host whose module fallback path is `ebin` (a stdlib `ebin` directory).
    pub fn new(ebin: impl Into<PathBuf>) -> ModuleHost {
        ModuleHost {
            arena: MinimalContext::new(),
            atoms: AtomTable::new(),
            modules: HashMap::new(),
            explicit: HashMap::new(),
            ebin: ebin.into(),
            search: Vec::new(),
            machines: Vec::new(),
        }
    }
    /// Registers `name` to load from a specific `.beam` path (the harness).
    pub fn register(&mut self, name: &str, path: impl Into<PathBuf>) {
        self.explicit.insert(name.to_string(), path.into());
    }

    /// Adds `dir` to the fallback search path (searched after the primary
    /// `ebin`), so a module living in another application's `ebin` — e.g.
    /// `kernel` — can be loaded by name. Additive: an unregistered directory is
    /// never searched.
    pub fn add_search_dir(&mut self, dir: impl Into<PathBuf>) {
        self.search.push(dir.into());
    }

    /// The `.beam` path for `name`: an explicit registration, else the primary
    /// `ebin`, else the first search directory that holds it (falling back to the
    /// primary path so a genuine miss still reports a file-not-found error).
    fn module_path(&self, name: &str) -> PathBuf {
        if let Some(path) = self.explicit.get(name) {
            return path.clone();
        }
        let file = format!("{name}.beam");
        let primary = self.ebin.join(&file);
        if primary.is_file() {
            return primary;
        }
        for dir in &self.search {
            let candidate = dir.join(&file);
            if candidate.is_file() {
                return candidate;
            }
        }
        primary
    }

    /// The one shared atom table (for `beam_term::cmp` in assertions).
    pub fn atom_table(&self) -> &AtomTable {
        &self.atoms
    }

    /// Interns `name` in the shared table and returns its atom term.
    pub fn intern(&mut self, name: &str) -> Eterm {
        make_atom(self.atoms.intern(name))
    }

    /// Loads `name` (memoized). A registered module uses its explicit path;
    /// otherwise the primary `ebin`, then any search directory, is tried.
    pub fn load(&mut self, name: &str) -> Result<Arc<Module>> {
        if let Some(module) = self.modules.get(name) {
            return Ok(module.clone());
        }
        let path = self.module_path(name);
        let bytes = std::fs::read(&path)?;
        let module = Arc::new(load_with_shared_atoms(&bytes, &mut self.atoms)?);
        self.modules.insert(name.to_string(), module.clone());
        Ok(module)
    }

    /// Loads the `.beam` at `path` under the module name `name` (memoized),
    /// bypassing the `ebin`/search-path lookup — the hook a directory **sweep**
    /// uses to load *every* `*.beam` it finds, regardless of where it lives.
    /// `name` is what later `call`/external-call lookups use.
    pub fn load_from_path(&mut self, name: &str, path: impl Into<PathBuf>) -> Result<Arc<Module>> {
        if let Some(module) = self.modules.get(name) {
            return Ok(module.clone());
        }
        let bytes = std::fs::read(path.into())?;
        let module = Arc::new(load_with_shared_atoms(&bytes, &mut self.atoms)?);
        self.modules.insert(name.to_string(), module.clone());
        Ok(module)
    }

    /// Runs the exported `module:function/args.len()` on a fresh machine sharing
    /// this context.
    pub fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let module = self.load(module)?;
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or(BeamError::BadArg)?;
        let mut machine = Machine::new();
        let result = machine.run_export(&module, function_atom, args.len() as u32, args, self);
        self.retain(machine);
        result
    }

    /// Keeps `machine` (and therefore every off-heap binary payload its run built)
    /// alive for the rest of the host's lifetime — see the `machines` field.
    fn retain(&mut self, machine: Machine) {
        self.machines.push(machine);
    }
    /// Run the fun `fun` on its defining module (the `call_fun`/`call_fun2`
    /// path).
    ///
    /// A `.beam` fun inlines only its `uniq` (per-module) and `index` (the
    /// module-level lambda index); together they identify the `FunT` descriptor
    /// [`FunDescriptor`] — and hence the defining module and entry label —
    /// unambiguously.
    ///
    /// The register file is set up exactly as the C emulator's `call_fun` does
    /// (`../../erts/emulator/beam/beam_common.c:1631-1633`,
    /// `reg[i + arity] = funp->env[i]`): the **call arguments** occupy
    /// `x(0..arity)` and the closure's **free variables** follow at
    /// `x(arity..arity+num_free)` — arguments first, then captures. A lambda's
    /// non-constant free variables are therefore its *trailing* compiled
    /// parameters (`-f/1-fun-0-/3` reads its one argument from `x0` and its two
    /// captures from `x1`/`x2`).
    fn apply_resolved(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        let uniq = terms::fun_uniq(fun)?;
        let index = terms::fun_index(fun)?;
        let free = terms::fun_free(fun)?.to_vec();

        let mut found: Option<(Arc<Module>, FunDescriptor)> = None;
        for module in self.modules.values() {
            if let Some(descriptor) = module
                .funs()
                .iter()
                .find(|d| u64::from(d.uniq) == uniq && d.index == index)
            {
                found = Some((module.clone(), *descriptor));
                break;
            }
        }
        let (module, descriptor) = found.ok_or(BeamError::BadArg)?;

        let mut combined = args.to_vec();
        combined.extend_from_slice(&free);
        if combined.len() != descriptor.arity as usize {
            return Err(BeamError::BadArity);
        }

        let mut machine = Machine::new();
        let result = machine.run_label(&module, descriptor.label, descriptor.arity, &combined, self);
        self.retain(machine);
        result
    }
}
// ---------------------------------------------------------------------------
// ExecutionContext
// ---------------------------------------------------------------------------

impl ExecutionContext for ModuleHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if let Some(result) = self.try_local_bif(module, function, arity, args) {
            return result;
        }
        // Not a built-in: an external module call. `erlang:*` is the BIF
        // namespace, never a loadable module, so an unknown `erlang` BIF is a
        // typed error rather than a spurious module load.
        if module == "erlang" {
            // Diagnostic: a blocked target is often blocked by one unserved
            // `erlang` BIF deep inside a module. Set `ERTS_TRACE_UNSERVED_BIF=1`
            // to print the exact BIF that stopped the run.
            if env::var_os("ERTS_TRACE_UNSERVED_BIF").is_some() {
                eprintln!("unserved BIF: erlang:{function}/{arity}");
            }
            return Err(BeamError::BadArg);
        }
        let module = self.load(module)?;
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or(BeamError::BadArg)?;
        let mut machine = Machine::new();
        let result = machine.run_export(&module, function_atom, arity, args, self);
        self.retain(machine);
        result
    }

    fn ensure_heap(        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.apply_resolved(fun, args)
    }
}

// ---------------------------------------------------------------------------
// The local BIF floor
// ---------------------------------------------------------------------------

/// A binary arithmetic operator.
#[derive(Clone, Copy)]
enum Arith {
    Add,
    Sub,
    Mul,
    Div,
}

/// A binary comparison operator (the `erlang` BIFs a boolean-returning guard or
/// expression compiles to).
#[derive(Clone, Copy)]
enum Compare {
    /// `==` (arithmetic ordering).
    Eq,
    /// `/=`
    Ne,
    /// `=:=` (exact ordering).
    EqExact,
    /// `=/=`
    NeExact,
    /// `<`
    Lt,
    /// `=<`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A bitwise integer operator (the `erlang` `band`/`bor`/`bxor` BIFs).
#[derive(Clone, Copy)]
enum BitOp {
    And,
    Or,
    Xor,
}

/// A number, as an arithmetic BIF sees it.
#[derive(Clone, Copy)]
enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn as_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }
}

/// Interpret a term as a number: a small integer or a boxed float, else `None`.
fn number(term: Eterm) -> Option<Number> {
    if is_small(term) {
        return Some(Number::Int(signed_value(term)));
    }
    terms::float_value(term).ok().map(Number::Float)
}

impl ModuleHost {
    /// The built-in BIF floor this host serves directly; `None` means "not a
    /// local BIF" (an external module call).
    fn try_local_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (module, function, arity) {
            ("erlang", "+", 2) => Some(self.arith(args, Arith::Add)),
            ("erlang", "-", 2) => Some(self.arith(args, Arith::Sub)),
            ("erlang", "*", 2) => Some(self.arith(args, Arith::Mul)),
            ("erlang", "div", 2) => Some(self.arith(args, Arith::Div)),
            ("erlang", "==", 2) => Some(self.compare(args, Compare::Eq)),
            ("erlang", "/=", 2) => Some(self.compare(args, Compare::Ne)),
            ("erlang", "=:=", 2) => Some(self.compare(args, Compare::EqExact)),
            ("erlang", "=/=", 2) => Some(self.compare(args, Compare::NeExact)),
            ("erlang", "<", 2) => Some(self.compare(args, Compare::Lt)),
            ("erlang", "=<", 2) => Some(self.compare(args, Compare::Le)),
            ("erlang", ">", 2) => Some(self.compare(args, Compare::Gt)),
            ("erlang", ">=", 2) => Some(self.compare(args, Compare::Ge)),
            ("erlang", "element", 2) => Some(element(args)),
            // `setelement(Index, Tuple, Value)` — `dict` rebuilds its segment
            // tuples with it (`on_bucket`/`put_bucket_s`).
            ("erlang", "setelement", 3) => Some(self.setelement(args)),
            ("erlang", "tuple_size", 1) => Some(tuple_size(args)),
            ("erlang", "length", 1) => Some(length(args)),
            // `hd/1` — the head of a cons cell (`calendar`/`unicode` walk lists
            // with it).
            ("erlang", "hd", 1) => Some(hd(args)),
            // `X rem Y` — the integer remainder (`calendar` computes a weekday
            // from a day count with it).
            ("erlang", "rem", 2) => Some(self.rem_bif(args)),
            // `list_to_tuple/1` — `array` and `calendar` build tuples from lists.
            ("erlang", "list_to_tuple", 1) => Some(self.list_to_tuple(args)),
            // `atom_to_list/1` — `filename` turns path components (atoms) into
            // strings with it.
            ("erlang", "atom_to_list", 1) => Some(self.atom_to_list(args)),
            // `phash/2` — the portable hash `dict` buckets its keys with.
            ("erlang", "phash", 2) => Some(self.phash(args)),
            // `system_info/1` — `filename` branches on `os_type`.
            ("erlang", "system_info", 1) => Some(self.system_info(args)),
            ("erlang", "++", 2) => Some(self.append(args)),
            ("erlang", "--", 2) => Some(self.subtract(args)),
            // Bit shifts: `gb_trees`/`gb_sets`/`array` size arithmetic uses them.
            ("erlang", "bsl", 2) => Some(self.bit_shift(args, true)),
            ("erlang", "bsr", 2) => Some(self.bit_shift(args, false)),
            // The bitwise operators on small integers (`beam_asm` masks opcodes
            // with `band`/`bor`).
            ("erlang", "band", 2) => Some(self.bit_op(args, BitOp::And)),
            ("erlang", "bor", 2) => Some(self.bit_op(args, BitOp::Or)),
            ("erlang", "bxor", 2) => Some(self.bit_op(args, BitOp::Xor)),
            ("erlang", "bnot", 1) => Some(self.bit_not(args)),
            // `tuple_to_list/1` / `make_tuple/2` — the tuple<->list conversions a
            // handful of modules (`file_sorter`, `erl_abstract_code`) use.
            ("erlang", "tuple_to_list", 1) => Some(self.tuple_to_list(args)),
            ("erlang", "make_tuple", 2) => Some(self.make_tuple(args)),
            // Binary <-> text conversions: `atom_to_binary/1`,
            // `iolist_to_binary/1`, `list_to_binary/1` and the mirror
            // `binary_to_list/1` (`erl_posix_msg`/`re`/`shell_docs_markdown` build
            // short binaries with them).
            ("erlang", "atom_to_binary", 1) => Some(self.atom_to_binary(args)),
            ("erlang", "iolist_to_binary", 1) => Some(self.iolist_to_binary(args)),
            ("erlang", "list_to_binary", 1) => Some(self.list_to_binary(args)),
            ("erlang", "binary_to_list", 1) => Some(self.binary_to_list(args)),
            // `node/0` — `nonode@nohost` for a non-distributed node, the value
            // `net` branches on.
            ("erlang", "node", 0) => Some(self.node_name(args)),
            // Term-ordering `max`/`min`: `gb_trees` balancing picks the larger of
            // two heights with `max/2`.
            ("erlang", "max", 2) => Some(self.max_min(args, true)),
            ("erlang", "min", 2) => Some(self.max_min(args, false)),
            // `lists:reverse/2` is a NIF in `lists.beam` (its `.beam` body is
            // `erlang:nif_error(undef)`), reached from `lists:reverse/1` for a
            // list of three or more elements.
            ("lists", "reverse", 2) => Some(self.reverse2(args)),
            // `lists:member/2` is likewise a `lists` NIF (exact `=:=` membership);
            // `queue:member/2` forwards to it.
            ("lists", "member", 2) => Some(self.list_member(args)),
            // These raise on the invalid-argument paths the tests deliberately
            // avoid; a typed error is the correct outward behavior.
            ("erlang", "error", 1) | ("erlang", "error", 2) | ("erlang", "error", 3) => {
                Some(Err(BeamError::BadArg))
            }
            ("erlang", "nif_error", 1) => Some(Err(BeamError::BadArg)),
            _ => None,
        }
    }

    /// The `true`/`false` atom (interned in the shared table).
    fn boolean(&mut self, value: bool) -> Eterm {
        self.intern(if value { "true" } else { "false" })
    }

    /// `+`/`-`/`*`/`div`: integer op integer stays integer; any float operand
    /// (except `div`) produces a boxed float. Mirrors the ERTS number tower for
    /// the small-int/float cases these modules reach.
    fn arith(&mut self, args: &[Eterm], op: Arith) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let a = number(args[0]).ok_or(BeamError::BadArith)?;
        let b = number(args[1]).ok_or(BeamError::BadArith)?;
        match (a, b) {
            (Number::Int(x), Number::Int(y)) => match op {
                Arith::Add => Ok(make_small(x + y)),
                Arith::Sub => Ok(make_small(x - y)),
                Arith::Mul => Ok(make_small(x * y)),
                Arith::Div => {
                    if y == 0 {
                        Err(BeamError::BadArith)
                    } else {
                        Ok(make_small(x.div_euclid(y)))
                    }
                }
            },
            // `div` is integer-only in Erlang; any float operand is a `badarith`.
            (_, _) if matches!(op, Arith::Div) => Err(BeamError::BadArith),
            (x, y) => {
                let (x, y) = (x.as_f64(), y.as_f64());
                let value = match op {
                    Arith::Add => x + y,
                    Arith::Sub => x - y,
                    Arith::Mul => x * y,
                    Arith::Div => unreachable!("guarded by the `Div` arm above"),
                };
                terms::make_float(self, value)
            }
        }
    }

    /// A comparison BIF, returning the `true`/`false` atom. `<`/`=<`/`>`/`>=`
    /// and `==`/`/=` use the arithmetic ordering; `=:=`/`=/=` the exact one.
    fn compare(&mut self, args: &[Eterm], op: Compare) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let ordering = match op {
            Compare::EqExact | Compare::NeExact => {
                cmp(args[0], args[1], &self.atoms)
            }
            _ => cmp_arith(args[0], args[1], &self.atoms),
        };
        let result = match op {
            Compare::Eq | Compare::EqExact => ordering.is_eq(),
            Compare::Ne | Compare::NeExact => !ordering.is_eq(),
            Compare::Lt => ordering.is_lt(),
            Compare::Le => ordering.is_le(),
            Compare::Gt => ordering.is_gt(),
            Compare::Ge => ordering.is_ge(),
        };
        Ok(self.boolean(result))
    }

    /// `A ++ B` — `A`'s elements prepended onto `B`.
    fn append(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let elements = terms::list_elements(args[0])?;
        let mut acc = args[1];
        for element in elements.iter().rev() {
            acc = terms::make_cons(self, *element, acc)?;
        }
        Ok(acc)
    }

    /// `lists:reverse(List, Tail)` (the `lists` NIF): `List` reversed with `Tail`
    /// appended (`reverse(List) ++ Tail`), so the elements are consed back on in
    /// their original order.
    fn reverse2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let elements = terms::list_elements(args[0])?;
        let mut acc = args[1];
        for element in elements.iter() {
            acc = terms::make_cons(self, *element, acc)?;
        }
        Ok(acc)
    }

    /// `A -- B` (the `erlang` list-subtraction BIF, reached via
    /// `lists:subtract/2`): remove from `A` the first occurrence of each element
    /// of `B`, compared with **exact** (`=:=`) equality — the real semantics
    /// (`[1.0] -- [1]` is `[1.0]`, `[2] -- [2.0]` is `[2]`).
    fn subtract(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let mut remaining = terms::list_elements(args[0])?;
        for &remove in terms::list_elements(args[1])?.iter() {
            if let Some(position) = remaining
                .iter()
                .position(|&element| cmp(element, remove, &self.atoms).is_eq())
            {
                remaining.remove(position);
            }
        }
        terms::make_list(self, &remaining)
    }

    /// `X bsl N` (`left_named = true`) / `X bsr N` — an integer bit shift with
    /// Erlang's negative-count semantics (a negative count shifts the other way).
    ///
    /// M1 has no bignums, so a shift whose result leaves the small-integer range
    /// is a typed [`BeamError::SystemLimit`] rather than a silently-mis-computed
    /// value; the module functions these tests run never reach that path.
    fn bit_shift(&mut self, args: &[Eterm], left_named: bool) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let (value, count) = match (number(args[0]), number(args[1])) {
            (Some(Number::Int(value)), Some(Number::Int(count))) => (value, count),
            _ => return Err(BeamError::BadArith),
        };
        let shift_left = if left_named { count >= 0 } else { count < 0 };
        // Clamp the shift count to the `i128` width; a genuinely larger left shift
        // then overflows the range check below (and a huge right shift is still
        // `0`/`-1`).
        let amount = count.unsigned_abs().min(127);
        let result = if shift_left {
            i128::from(value) << amount
        } else {
            i128::from(value) >> amount
        };
        if result < i128::from(MIN_SMALL) || result > i128::from(MAX_SMALL) {
            return Err(BeamError::SystemLimit);
        }
        Ok(make_small(result as i64))
    }

    /// `erlang:max/2` / `erlang:min/2` — the larger / smaller of two terms under
    /// the **arithmetic** ordering, returning the first term when the two compare
    /// equal (the real semantics: `max(1, 1.0)` is `1`).
    fn max_min(&mut self, args: &[Eterm], want_max: bool) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let ordering = cmp_arith(args[0], args[1], &self.atoms);
        let take_first = if want_max {
            !ordering.is_lt()
        } else {
            !ordering.is_gt()
        };
        Ok(if take_first { args[0] } else { args[1] })
    }

    /// `lists:member(Elem, List)` (the `lists` NIF): `true` when `Elem` occurs in
    /// `List` under **exact** (`=:=`) equality — `lists:member(1.0, [1])` is
    /// `false`.
    fn list_member(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let found = terms::list_elements(args[1])?
            .iter()
            .any(|&element| cmp(element, args[0], &self.atoms).is_eq());
        Ok(self.boolean(found))
    }

    /// `X rem Y` — the integer remainder. The result carries the *dividend's* sign
    /// (Rust's `%` truncates toward zero, exactly as Erlang's `rem` does, so
    /// `-7 rem 2` is `-1`). `rem(X, 0)` and any non-integer operand is a
    /// `badarith`, matching the real BIF.
    fn rem_bif(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        match (number(args[0]), number(args[1])) {
            (Some(Number::Int(x)), Some(Number::Int(y))) => {
                if y == 0 {
                    Err(BeamError::BadArith)
                } else {
                    Ok(make_small(x % y))
                }
            }
            _ => Err(BeamError::BadArith),
        }
    }

    /// `list_to_tuple(List)` — a (proper) list as a tuple. A non-list operand is a
    /// `badarg` (the real BIF raises `badarg`).
    fn list_to_tuple(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = args.first().ok_or(BeamError::BadArg)?;
        let elements = terms::list_elements(*list)?;
        terms::make_tuple(self, &elements)
    }

    /// `setelement(Index, Tuple, Value)` — a **new** tuple with the 1-based
    /// `Index` replaced by `Value` (the source is copied, exactly as the real BIF
    /// behaves). A non-integer index, an index outside `1..=tuple_size(Tuple)`, or
    /// a non-tuple operand is a `badarg`.
    fn setelement(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 3 || !is_small(args[0]) {
            return Err(BeamError::BadArg);
        }
        let index = signed_value(args[0]);
        let arity = terms::tuple_arity(args[1])?;
        if index < 1 || index as usize > arity {
            return Err(BeamError::BadArg);
        }
        let mut elements = Vec::with_capacity(arity);
        for offset in 0..arity {
            elements.push(if offset + 1 == index as usize {
                args[2]
            } else {
                terms::tuple_element(args[1], offset)?
            });
        }
        terms::make_tuple(self, &elements)
    }

    /// `atom_to_list(Atom)` — the atom's name as a list of code points (ASCII for
    /// every atom these tests use). A non-atom operand is a `badarg`.
    fn atom_to_list(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let atom = args.first().copied().ok_or(BeamError::BadArg)?;
        if !is_atom(atom) {
            return Err(BeamError::BadArg);
        }
        let name = self
            .atoms
            .name(atom_val(atom))
            .ok_or(BeamError::BadArg)?
            .to_string();
        let chars: Vec<Eterm> = name.chars().map(|c| make_small(i64::from(c as u32))).collect();
        terms::make_list(self, &chars)
    }

    /// `tuple_to_list(Tuple)` — the tuple's elements as a list. A non-tuple is a
    /// `badarg`.
    fn tuple_to_list(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let tuple = args.first().copied().ok_or(BeamError::BadArg)?;
        let arity = terms::tuple_arity(tuple)?;
        let mut elements = Vec::with_capacity(arity);
        for offset in 0..arity {
            elements.push(terms::tuple_element(tuple, offset)?);
        }
        terms::make_list(self, &elements)
    }

    /// `make_tuple(Arity, Init)` — a tuple of `Arity` copies of `Init`. A
    /// negative or non-integer arity, or one above [`MAX_ARITYVAL`], is a `badarg`.
    fn make_tuple(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 || !is_small(args[0]) {
            return Err(BeamError::BadArg);
        }
        let arity = signed_value(args[0]);
        if arity < 0 || arity as u64 > MAX_ARITYVAL as u64 {
            return Err(BeamError::BadArg);
        }
        let elements = vec![args[1]; arity as usize];
        terms::make_tuple(self, &elements)
    }

    /// `atom_to_binary(Atom)` — the atom's name as a UTF-8 binary (the atom must
    /// be a valid Latin-1/Unicode name and fit the host's on-heap binary limit).
    fn atom_to_binary(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let atom = args.first().copied().ok_or(BeamError::BadArg)?;
        if !is_atom(atom) {
            return Err(BeamError::BadArg);
        }
        let name = self
            .atoms
            .name(atom_val(atom))
            .ok_or(BeamError::BadArg)?
            .to_string();
        terms::make_heap_binary(self, name.as_bytes())
    }

    /// `iolist_to_binary(IoList)` — an iolist (nested lists of bytes and
    /// binaries) as one binary. An out-of-range byte, an improper tail or a
    /// non-iolist element is a `badarg`; a result wider than the host's on-heap
    /// binary limit is the typed unsupported error.
    fn iolist_to_binary(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = args.first().copied().ok_or(BeamError::BadArg)?;
        let mut bytes = Vec::new();
        self.collect_iolist(list, &mut bytes)?;
        terms::make_heap_binary(self, &bytes)
    }

    /// `list_to_binary(List)` — a list of bytes as a binary. An out-of-range
    /// element or an improper list is a `badarg`.
    fn list_to_binary(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = args.first().copied().ok_or(BeamError::BadArg)?;
        let elements = terms::list_elements(list)?;
        let mut bytes = Vec::with_capacity(elements.len());
        for element in elements {
            if !is_small(element) {
                return Err(BeamError::BadArg);
            }
            let byte = signed_value(element);
            if !(0..=255).contains(&byte) {
                return Err(BeamError::BadArg);
            }
            bytes.push(byte as u8);
        }
        terms::make_heap_binary(self, &bytes)
    }

    /// `binary_to_list(Bin)` — a binary's bytes as a list. A non-binary is a
    /// `badarg`.
    fn binary_to_list(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let binary = args.first().copied().ok_or(BeamError::BadArg)?;
        let bytes = terms::binary_bytes(binary)?;
        let chars: Vec<Eterm> = bytes.iter().map(|&b| make_small(i64::from(b))).collect();
        terms::make_list(self, &chars)
    }

    /// `node()` — the node's name, `nonode@nohost` for this host's
    /// non-distributed setting (the value `net` branches on).
    fn node_name(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if !args.is_empty() {
            return Err(BeamError::BadArg);
        }
        Ok(self.intern("nonode@nohost"))
    }

    /// Appends the bytes of an iolist to `out`. Stops early (with the typed
    /// unsupported error) once the total exceeds the host's on-heap binary limit,
    /// since such a result could not be represented anyway.
    fn collect_iolist(&mut self, term: Eterm, out: &mut Vec<u8>) -> Result<()> {
        if out.len() > ERL_ONHEAP_BINARY_LIMIT {
            return Err(beam_engine::unsupported("heap binary > 64 bytes"));
        }
        if is_small(term) {
            let byte = signed_value(term);
            if !(0..=255).contains(&byte) {
                return Err(BeamError::BadArg);
            }
            out.push(byte as u8);
            return Ok(());
        }
        if let Ok(bytes) = terms::binary_bytes(term) {
            out.extend_from_slice(bytes);
            return Ok(());
        }
        if beam_term::is_nil(term) {
            return Ok(());
        }
        if beam_term::is_list(term) {
            let mut current = term;
            loop {
                let head = terms::cons_head(current)?;
                self.collect_iolist(head, out)?;
                let tail = terms::cons_tail(current)?;
                if beam_term::is_nil(tail) {
                    return Ok(());
                }
                if !beam_term::is_list(tail) {
                    return Err(BeamError::BadArg);
                }
                current = tail;
            }
        }
        Err(BeamError::BadArg)
    }

    /// `X band Y` / `X bor Y` / `X bxor Y` on small integers. A non-integer
    /// operand is a `badarith`.
    fn bit_op(&mut self, args: &[Eterm], op: BitOp) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let (x, y) = match (number(args[0]), number(args[1])) {
            (Some(Number::Int(x)), Some(Number::Int(y))) => (x, y),
            _ => return Err(BeamError::BadArith),
        };
        Ok(make_small(match op {
            BitOp::And => x & y,
            BitOp::Or => x | y,
            BitOp::Xor => x ^ y,
        }))
    }

    /// `bnot X` — the bitwise complement of a small integer. A non-integer
    /// operand is a `badarith`.
    fn bit_not(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 1 {
            return Err(BeamError::BadArg);
        }
        match number(args[0]) {
            Some(Number::Int(x)) => Ok(make_small(!x)),
            _ => Err(BeamError::BadArith),
        }
    }

    /// `phash(Term, Range)` — the portable hash `dict` buckets its keys with,
    /// returned in `1..=Range`.
    ///
    /// The exact `erts_phash` bit pattern is not reproduced (it is not an
    /// externally pinned value); a **deterministic structural hash** is used
    /// instead. That is observationally equivalent for every `dict` operation these
    /// tests exercise (`size`/`fetch`/`is_key`, which do not expose the bucket
    /// order): `dict` only ever compares hashes it computed with the *same*
    /// function. A non-positive or non-integer `Range` is a `badarg`.
    fn phash(&mut self, args: &[Eterm]) -> Result<Eterm> {
        if args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let range = match number(args[1]) {
            Some(Number::Int(range)) if range > 0 => range,
            _ => return Err(BeamError::BadArg),
        };
        let hash = beam_term::hashing::term_hash(args[0], &self.atoms);
        Ok(make_small((hash % range as u64) as i64 + 1))
    }

    /// `system_info(Key)` — the few options the real modules this host runs
    /// consult. Only `os_type` is served (as `{Family, Name}` derived from the
    /// **host** OS, so `filename` takes the same path-syntax branch the local
    /// oracle does); every other option is a `badarg`, exactly like an unknown BIF.
    fn system_info(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let key = args.first().copied().ok_or(BeamError::BadArg)?;
        if !is_atom(key) {
            return Err(BeamError::BadArg);
        }
        match self.atoms.name(atom_val(key)) {
            Some("os_type") => {
                let (family, name) = match std::env::consts::OS {
                    "macos" => ("unix", "darwin"),
                    "windows" => ("win32", "nt"),
                    "linux" => ("unix", "linux"),
                    _ => ("unix", "linux"),
                };
                let family = self.intern(family);
                let name = self.intern(name);
                terms::make_tuple(self, &[family, name])
            }
            _ => Err(BeamError::BadArg),
        }
    }
}

/// `erlang:element/2` — the 1-based element of a tuple.
fn element(args: &[Eterm]) -> Result<Eterm> {
    if args.len() != 2 || !is_small(args[0]) {
        return Err(BeamError::BadArg);
    }
    let index = signed_value(args[0]);
    if index < 1 {
        return Err(BeamError::BadArg);
    }
    terms::tuple_element(args[1], (index - 1) as usize)
}

/// `erlang:length/1` — the length of a proper list.
fn length(args: &[Eterm]) -> Result<Eterm> {
    let list = args.first().ok_or(BeamError::BadArg)?;
    Ok(make_small(terms::list_elements(*list)?.len() as i64))
}

/// `erlang:tuple_size/1` — the number of elements of a tuple.
fn tuple_size(args: &[Eterm]) -> Result<Eterm> {
    let tuple = args.first().ok_or(BeamError::BadArg)?;
    Ok(make_small(terms::tuple_arity(*tuple)? as i64))
}

/// `erlang:hd/1` — the head of a cons cell; anything else is a `badarg`.
fn hd(args: &[Eterm]) -> Result<Eterm> {
    let list = args.first().ok_or(BeamError::BadArg)?;
    terms::cons_head(*list)
}

// ---------------------------------------------------------------------------
// Small list helpers shared by the tests
// ---------------------------------------------------------------------------

/// Builds a proper list of small integers.
pub fn int_list(host: &mut ModuleHost, values: &[i64]) -> Eterm {
    let elements: Vec<Eterm> = values.iter().map(|&v| make_small(v)).collect();
    terms::make_list(host, &elements).expect("build an int list")
}

/// An Erlang string (a list of character code points) as a term.
pub fn chars(host: &mut ModuleHost, text: &str) -> Eterm {
    let codes: Vec<i64> = text.chars().map(|c| i64::from(c as u32)).collect();
    int_list(host, &codes)
}

/// Asserts `got` is the Erlang string `want` (a list of character code points).
pub fn assert_str(host: &ModuleHost, got: Eterm, want: &str) {
    let codes: Vec<i64> = want.chars().map(|c| i64::from(c as u32)).collect();
    assert_int_list(host, got, &codes);
}

/// Asserts `got` is the proper list of the small integers `want`, comparing with
/// `beam_term::cmp` (the module's exact `=:=` ordering).
pub fn assert_int_list(host: &ModuleHost, got: Eterm, want: &[i64]) {
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(
        elements.len(),
        want.len(),
        "list length: got {elements:?}, want {want:?}"
    );
    for (index, (&got, &want)) in elements.iter().zip(want.iter()).enumerate() {
        let expected = make_small(want);
        assert!(
            cmp(got, expected, host.atom_table()).is_eq(),
            "element {index}: got {got:?}, want {want}"
        );
    }
}

/// Asserts `got` is the shared atom `name` (resolved by name through the shared
/// table, so a term interned by any module compares equal).
pub fn assert_atom(host: &ModuleHost, got: Eterm, name: &str) {
    let atom = host
        .atom_table()
        .lookup(name)
        .unwrap_or_else(|| panic!("the shared atom table does not name `{name}`"));
    let expected = make_atom(atom);
    assert!(
        cmp(got, expected, host.atom_table()).is_eq(),
        "got {got:?}, want the atom `{name}`"
    );
}

/// The two-element tuple `{Key, Value}` where `Key` is the shared atom `key` and
/// `Value` the small integer `value` — the shape `proplists` operates on.
pub fn keyed_tuple(host: &mut ModuleHost, key: &str, value: i64) -> Eterm {
    let key = host.intern(key);
    terms::make_tuple(host, &[key, make_small(value)]).expect("build a 2-tuple")
}

/// Asserts `got` is the list of `{atom, small integer}` tuples `want`.
pub fn assert_keyed_list(host: &ModuleHost, got: Eterm, want: &[(&str, i64)]) {
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(
        elements.len(),
        want.len(),
        "list length: got {elements:?}, want {want:?}"
    );
    for (index, (&got, &(key, value))) in elements.iter().zip(want.iter()).enumerate() {
        assert_eq!(
            terms::tuple_arity(got).ok(),
            Some(2),
            "element {index}: not a 2-tuple: {got:?}"
        );
        let got_key = terms::tuple_element(got, 0).expect("tuple key");
        let got_value = terms::tuple_element(got, 1).expect("tuple value");
        assert_atom(host, got_key, key);
        assert!(
            cmp(got_value, make_small(value), host.atom_table()).is_eq(),
            "element {index}: got {got_value:?}, want {value}"
        );
    }
}

/// Builds a tuple from the given terms (the test-side `put_tuple2`).
pub fn term_tuple(host: &mut ModuleHost, elems: &[Eterm]) -> Eterm {
    terms::make_tuple(host, elems).expect("build a tuple")
}

/// Builds a proper list from the given terms.
pub fn term_list(host: &mut ModuleHost, elems: &[Eterm]) -> Eterm {
    terms::make_list(host, elems).expect("build a list")
}

/// The shared atom `name` as a term (interning it if no module has yet).
pub fn term_atom(host: &mut ModuleHost, name: &str) -> Eterm {
    host.intern(name)
}

/// Asserts `got` and `want` are the same term under the module's exact (`=:=`)
/// ordering.
pub fn assert_term(host: &ModuleHost, got: Eterm, want: Eterm) {
    assert!(
        cmp(got, want, host.atom_table()).is_eq(),
        "got {got:?}, want {want:?}"
    );
}

/// Asserts `got` is the small integer `want`.
pub fn assert_small(host: &ModuleHost, got: Eterm, want: i64) {
    assert_term(host, got, make_small(want));
}

/// Asserts `got` is the proper list of `{integer, integer}` 2-tuples `want`.
pub fn assert_int_pair_list(host: &ModuleHost, got: Eterm, want: &[(i64, i64)]) {
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(
        elements.len(),
        want.len(),
        "list length: got {elements:?}, want {want:?}"
    );
    for (index, (&got, &(first, second))) in elements.iter().zip(want.iter()).enumerate() {
        assert_eq!(
            terms::tuple_arity(got).ok(),
            Some(2),
            "element {index}: not a 2-tuple: {got:?}"
        );
        assert_small(host, terms::tuple_element(got, 0).expect("pair key"), first);
        assert_small(host, terms::tuple_element(got, 1).expect("pair value"), second);
    }
}

/// Asserts `got` is the 2-tuple `{Rear, Front}` of the two integer lists — the
/// internal shape of a `queue:queue/0` as the real `queue` module represents it.
pub fn assert_int_queue(host: &ModuleHost, got: Eterm, rear: &[i64], front: &[i64]) {
    assert_eq!(
        terms::tuple_arity(got).ok(),
        Some(2),
        "a queue is a 2-tuple: {got:?}"
    );
    assert_int_list(host, terms::tuple_element(got, 0).expect("rear"), rear);
    assert_int_list(host, terms::tuple_element(got, 1).expect("front"), front);
}
