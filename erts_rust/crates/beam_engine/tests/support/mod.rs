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
    cmp, cmp_arith, is_small, make_atom, make_small, signed_value, AtomTable, Eterm,
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
    if let Ok(dir) = env::var("ERTS_STDLIB_DIR") {
        let dir = PathBuf::from(dir);
        if dir.join("lists.beam").is_file() {
            return Some(dir);
        }
        let ebin = dir.join("ebin");
        if ebin.join("lists.beam").is_file() {
            return Some(ebin);
        }
        return None;
    }

    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir(stdlib)]),halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8(output.stdout).ok()?;
    let ebin = PathBuf::from(dir.trim()).join("ebin");
    if ebin.join("lists.beam").is_file() {
        Some(ebin)
    } else {
        None
    }
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

/// Loads a `.beam` image into the IR, interning its atoms (and its decoded
/// literal atoms) into `atoms` — the host's one shared [`AtomTable`].
///
/// This is [`beam_loader::load`]'s exact pipeline, re-run through the loader's
/// public modules so the shared table (not a private per-module one) is the
/// interning context. See the module docs for why that matters.
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
    let module = lower::build_module(&chunks, &raw, atoms.clone())?;
    let base = atoms.len();
    for index in base..module.atom_table.len() {
        if let Some(name) = module.atom_table.name(Atom::from_raw(index as u32)) {
            atoms.intern(name);
        }
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
    /// The stdlib `ebin` directory used as the fallback code path.
    ebin: PathBuf,
}

impl ModuleHost {
    /// A host whose module fallback path is `ebin` (a stdlib `ebin` directory).
    pub fn new(ebin: impl Into<PathBuf>) -> ModuleHost {
        ModuleHost {
            arena: MinimalContext::new(),
            atoms: AtomTable::new(),
            modules: HashMap::new(),
            explicit: HashMap::new(),
            ebin: ebin.into(),
        }
    }

    /// Registers `name` to load from a specific `.beam` path (the harness).
    pub fn register(&mut self, name: &str, path: impl Into<PathBuf>) {
        self.explicit.insert(name.to_string(), path.into());
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
    /// otherwise `<ebin>/<name>.beam` is used.
    pub fn load(&mut self, name: &str) -> Result<Arc<Module>> {
        if let Some(module) = self.modules.get(name) {
            return Ok(module.clone());
        }
        let path = match self.explicit.get(name) {
            Some(path) => path.clone(),
            None => self.ebin.join(format!("{name}.beam")),
        };
        let bytes = std::fs::read(&path)?;
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
        machine.run_export(&module, function_atom, args.len() as u32, args, self)
    }

    /// Run the fun `fun` on its defining module (the `call_fun`/`call_fun2`
    /// path).
    ///
    /// A `.beam` fun inlines only its `uniq` (per-module) and `index` (the
    /// module-level lambda index); together they identify the `FunT` descriptor
    /// [`FunDescriptor`] — and hence the defining module and entry label —
    /// unambiguously. The closure body is entered with the free variables
    /// **first** followed by the call arguments (a lambda's captured variables
    /// are its leading parameters).
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

        let mut combined = free;
        combined.extend_from_slice(args);
        if combined.len() != descriptor.arity as usize {
            return Err(BeamError::BadArity);
        }

        let mut machine = Machine::new();
        machine.run_label(&module, descriptor.label, descriptor.arity, &combined, self)
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
            return Err(BeamError::BadArg);
        }
        let module = self.load(module)?;
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or(BeamError::BadArg)?;
        let mut machine = Machine::new();
        machine.run_export(&module, function_atom, arity, args, self)
    }

    fn ensure_heap(
        &mut self,
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
            ("erlang", "tuple_size", 1) => Some(tuple_size(args)),
            ("erlang", "length", 1) => Some(length(args)),
            ("erlang", "++", 2) => Some(self.append(args)),
            // `lists:reverse/2` is a NIF in `lists.beam` (its `.beam` body is
            // `erlang:nif_error(undef)`), reached from `lists:reverse/1` for a
            // list of three or more elements.
            ("lists", "reverse", 2) => Some(self.reverse2(args)),
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
    /// appended.
    fn reverse2(&mut self, args: &[Eterm]) -> Result<Eterm> {
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

// ---------------------------------------------------------------------------
// Small list helpers shared by the tests
// ---------------------------------------------------------------------------

/// Builds a proper list of small integers.
pub fn int_list(host: &mut ModuleHost, values: &[i64]) -> Eterm {
    let elements: Vec<Eterm> = values.iter().map(|&v| make_small(v)).collect();
    terms::make_list(host, &elements).expect("build an int list")
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
