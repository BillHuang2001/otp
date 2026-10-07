//! The **OTP boot-script reader and executor** (contract A.7, part 2).
//!
//! OTP starts a node by reading a *boot script*: the `.boot` files under the release's
//! `bin/` directory (`start.boot`, `start_clean.boot`, `no_dot_erlang.boot`). A boot
//! script is simply the `binary_to_term` image of
//!
//! ```text
//! {script, {NameString, VsnString}, [Instruction, ...]}
//! ```
//!
//! where every instruction is a directive the `init` process executes in order. This
//! module reads that image into a **pure-Rust** typed form ([`BootScript`]) and executes
//! it against a [`Runtime`]. It is the counterpart of
//! [`crate::boot::preloaded`] (which loads the preloaded module set): the preloaded set is
//! the emulator image, the boot script is the first *file* the runtime is told to read.
//!
//! # The directives
//!
//! The real `start.boot` (OTP 29/erts-17.1, 22 instructions) uses exactly these eight
//! directives, all of which this module models:
//!
//! | Directive | Meaning |
//! |---|---|
//! | `{preLoaded, [Mod]}` | the modules already in the emulator image |
//! | `{progress, Atom}` | an informational progress marker |
//! | `{path, [DirString]}` | prepend code-path directories (a leading `$ROOT` is expanded) |
//! | `{primLoad, [Mod]}` | load a batch of modules from the code path |
//! | `{kernel_load_completed}` | the kernel's initial load finished |
//! | `{module, Mod}` | load one module |
//! | `{kernelProcess, Name, {M,F,A}}` | spawn (and register) a kernel process |
//! | `{apply, {M,F,A}}` | call `M:F(A...)` |
//!
//! An instruction outside this set is a typed **capability gap** ([`crate::error::unsupported`]),
//! never a silent skip; a malformed image is a typed [`BeamError`](beam_common::BeamError).
//!
//! # Execution stops at the first directive that cannot run
//!
//! [`execute`] walks the instructions in order and returns the first error it meets —
//! loading a module that is not on the code path, spawning a process whose function is not
//! exported, or applying an MFA the runtime cannot run. This is the honest M1 behaviour for
//! a real `start.boot`: it loads every `primLoad` batch and then stops at the first
//! directive that needs machinery M1 does not have (`application:load/1`, say).
//!
//! It **never hangs**: it drives the engine's own entry points directly
//! (never [`Runtime::run_scheduler`](crate::runtime::Runtime::run_scheduler)), so a
//! blocking `receive` is folded into the engine's typed `error::blocked()` and an
//! unsupported opcode into a typed `unsupported` — both returned, never awaited.

use std::collections::HashMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_term::{
    atom_val, is_atom, is_list, is_nil, is_small, make_small, signed_value, AtomTable, Eterm,
};

use crate::context::RuntimeContext;
use crate::error;
use crate::runtime::Runtime;

/// A parsed boot script: its `{Name, Vsn}` header and its ordered instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootScript {
    /// The script's `Name` — `"Erlang/OTP"` for the real `start.boot`.
    pub name: String,
    /// The script's `Vsn` — `"30"` for this release.
    pub vsn: String,
    /// The directives, in execution order.
    pub instructions: Vec<Instruction>,
}

/// One boot-script directive.
///
/// The variants are the exact set the real `start.boot` uses (see the module docs); an
/// instruction outside it is a typed capability gap, so this enum is exhaustive by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    /// `{preLoaded, [Mod]}` — the modules of the emulator image.
    PreLoaded(Vec<String>),
    /// `{progress, Atom}` — an informational progress marker.
    Progress(String),
    /// `{path, [DirString]}` — code-path directories (a leading `$ROOT` is expanded).
    Path(Vec<String>),
    /// `{primLoad, [Mod]}` — a batch of modules to load from the code path.
    PrimLoad(Vec<String>),
    /// `{kernel_load_completed}` — the kernel's initial load finished.
    KernelLoadCompleted,
    /// `{module, Mod}` — load one module.
    Module(String),
    /// `{kernelProcess, Name, {M,F,A}}` — spawn (and register) a kernel process.
    KernelProcess {
        /// The name the process is registered under.
        name: String,
        /// The spawned process's entry point.
        mfa: Mfa,
    },
    /// `{apply, {M,F,A}}` — call `M:F(A...)`.
    Apply {
        /// The called function.
        mfa: Mfa,
    },
}

/// A fully applied function: module, function and already-*decoded* arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mfa {
    /// The module name.
    pub module: String,
    /// The function name.
    pub function: String,
    /// The call's arguments, as [`Value`]s.
    pub args: Vec<Value>,
}

/// One argument of an [`Mfa`].
///
/// The variants cover the literal term shapes a boot script's arguments use (atoms,
/// integers, strings, lists and tuples, arbitrarily nested). A term outside them — a pid,
/// fun, float, big integer, binary, map, … — is [`Value::Other`], which [`execute`] refuses
/// with a typed capability gap rather than fabricating a wrong term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// An atom, by name.
    Atom(String),
    /// A small integer.
    Int(i64),
    /// An Erlang string (a list whose every element is a character code).
    Str(String),
    /// A (possibly empty) proper list of values.
    List(Vec<Value>),
    /// A tuple of values.
    Tuple(Vec<Value>),
    /// Any other term — outside the [`execute`] literal set.
    Other,
}

/// Reads the boot script at `path` into a typed [`BootScript`].
///
/// The image is decoded with the workspace's ETF decoder ([`beam_codec::decode_term`],
/// which consumes the leading `VERSION_MAGIC`), then walked into the owned form above —
/// no `Eterm` outlives this call.
///
/// # Errors
///
/// * a read error for `path`;
/// * the decoder's typed error for a malformed image (a bad/absent version byte, a bad tag,
///   a truncated stream) or for one too large for the parse heap;
/// * a typed [`error::message`] when the decoded term is not a
///   `{script, {Name, Vsn}, [Instruction]}` shape, and a typed [`error::unsupported`] for a
///   well-shaped script whose directive this runtime does not model.
pub fn read(path: &Path) -> Result<BootScript> {
    let bytes = std::fs::read(path)
        .map_err(|source| error::message(format!("cannot read {}: {source}", path.display())))?;
    // A one-shot parse heap, generously sized from the image: the decoded term can be
    // several times the wire image (a list adds one 2-word cons cell per element).
    let mut heap = Heap::new(heap_words_for(bytes.len()))?;
    let mut atoms = AtomTable::new();
    let term = beam_codec::decode_term(&bytes, &mut heap, &mut atoms)?;
    parse_script(term, &atoms, path)
}

/// Executes `script` against `runtime`, stopping at the first directive that cannot run.
///
/// See the module docs for the exact semantics of every directive; the load-bearing rules
/// are:
///
/// * a module that is not on the code path is the code server's typed `error:undef`, never
///   a silent skip;
/// * a `$ROOT` in a `{path, ...}` entry is expanded from the runtime's resolved Erlang root;
/// * a [`Value::Other`] argument is a typed capability gap (it cannot be rebuilt faithfully);
/// * [`Instruction::Progress`]/[`Instruction::KernelLoadCompleted`] are informational and
///   never fail;
/// * a `{apply, ...}` that blocks, or whose code needs an opcode M1 does not execute,
///   returns the engine's typed error instead of hanging.
///
/// # Errors
///
/// The first directive's error (see above), propagated unchanged.
pub fn execute(runtime: &mut Runtime, script: &BootScript) -> Result<()> {
    // The script's `$ROOT` expansion and its module loads both need the system code path,
    // so the runtime is booted first (idempotent): a caller may hand `execute` a fresh
    // runtime, exactly as `Runtime::boot_from_script` does.
    runtime.boot()?;
    for instruction in &script.instructions {
        execute_instruction(runtime, instruction)?;
    }
    Ok(())
}

/// Executes one directive (the [`execute`] loop body).
fn execute_instruction(runtime: &mut Runtime, instruction: &Instruction) -> Result<()> {
    match instruction {
        // `preLoaded` names are already in the cache after `Runtime::boot`; loading again
        // is a memoized no-op. A name the runtime does not ship is still a typed error.
        Instruction::PreLoaded(modules) | Instruction::PrimLoad(modules) => {
            for name in modules {
                runtime.load_code_module(name)?;
            }
            Ok(())
        }
        Instruction::Module(name) => runtime.load_code_module(name),
        // Informational markers `init` uses for synchronisation; nothing to do.
        Instruction::Progress(_) | Instruction::KernelLoadCompleted => Ok(()),
        Instruction::Path(directories) => {
            for directory in directories {
                let expanded = expand_root(runtime, directory)?;
                runtime.add_code_path(expanded);
            }
            Ok(())
        }
        Instruction::KernelProcess { name, mfa } => {
            let args = boot_args(runtime, &mfa.args)?;
            let pid = runtime.spawn_process(&mfa.module, &mfa.function, args)?;
            runtime.register_boot_process(name, pid)
        }
        Instruction::Apply { mfa } => {
            let args = boot_args(runtime, &mfa.args)?;
            let mut context = RuntimeContext::new(runtime);
            let _ = context.call_bif(&mfa.module, &mfa.function, args.len() as u32, &args)?;
            Ok(())
        }
    }
}

/// Expands a leading `$ROOT` in a `{path, ...}` entry to the runtime's Erlang root.
///
/// `$ROOT` is the directory above `<lib>` (`CodeServer::lib_root`), so
/// `$ROOT/lib/kernel/ebin` becomes the kernel ebin the code path already holds. A path
/// without the prefix is returned unchanged; a `$ROOT` that cannot be resolved (no
/// `<lib>/<app>-<vsn>/ebin` on the code path) is a typed error, never a silent pass-through.
fn expand_root(runtime: &Runtime, directory: &str) -> Result<PathBuf> {
    let Some(rest) = directory.strip_prefix("$ROOT") else {
        return Ok(PathBuf::from(directory));
    };
    let lib_root = runtime.code_server().lib_root().ok_or_else(|| {
        error::message(format!(
            "boot script `$ROOT` in {directory} cannot be expanded: the code path has no \
             <lib>/<app>-<vsn>/ebin directory"
        ))
    })?;
    let root = lib_root.parent().ok_or_else(|| {
        error::message(format!(
            "boot script `$ROOT` in {directory} cannot be expanded: {} has no parent",
            lib_root.display()
        ))
    })?;
    Ok(root.join(rest.trim_start_matches('/')))
}

/// Materializes a directive's argument values onto the runtime's pointer-stable arena.
///
/// Every atom the argument tree mentions is interned **first**, so the tree can be built
/// while a [`RuntimeContext`] borrows the runtime (the context owns the term allocation).
fn boot_args(runtime: &mut Runtime, values: &[Value]) -> Result<Vec<Eterm>> {
    let atom_terms = intern_atoms(runtime, values);
    let mut context = RuntimeContext::new(runtime);
    values
        .iter()
        .map(|value| build_value(&mut context, value, &atom_terms))
        .collect()
}

/// Interns every atom name appearing in `values` into the shared table, returning the term
/// for each name (atoms are immediates, so the terms stay valid for the runtime's life).
fn intern_atoms(runtime: &mut Runtime, values: &[Value]) -> HashMap<String, Eterm> {
    let mut names: Vec<String> = Vec::new();
    for value in values {
        collect_atom_names(value, &mut names);
    }
    names
        .into_iter()
        .map(|name| {
            let term = runtime.intern(&name);
            (name, term)
        })
        .collect()
}

/// Collects the distinct atom names in `value` into `names` (order of first appearance).
fn collect_atom_names(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::Atom(name) => {
            if !names.iter().any(|seen| seen == name) {
                names.push(name.clone());
            }
        }
        Value::List(items) | Value::Tuple(items) => {
            for item in items {
                collect_atom_names(item, names);
            }
        }
        Value::Int(_) | Value::Str(_) | Value::Other => {}
    }
}

/// Builds one [`Value`] as a term on the context's heap.
fn build_value(
    context: &mut RuntimeContext<'_>,
    value: &Value,
    atoms: &HashMap<String, Eterm>,
) -> Result<Eterm> {
    Ok(match value {
        Value::Atom(name) => *atoms.get(name).ok_or_else(|| {
            error::message(format!("boot script atom `{name}` was not interned"))
        })?,
        Value::Int(value) => make_small(*value),
        Value::Str(text) => {
            let chars: Vec<Eterm> = text
                .chars()
                .map(|ch| make_small(i64::from(u32::from(ch))))
                .collect();
            terms::make_list(context, &chars)?
        }
        Value::List(items) => {
            let elements = items
                .iter()
                .map(|item| build_value(context, item, atoms))
                .collect::<Result<Vec<Eterm>>>()?;
            terms::make_list(context, &elements)?
        }
        Value::Tuple(items) => {
            let elements = items
                .iter()
                .map(|item| build_value(context, item, atoms))
                .collect::<Result<Vec<Eterm>>>()?;
            terms::make_tuple(context, &elements)?
        }
        Value::Other => {
            return Err(error::unsupported(
                "boot-script argument term outside the supported literal set",
            ))
        }
    })
}

// ---------------------------------------------------------------------------
// Walking the decoded term into the typed form
// ---------------------------------------------------------------------------

/// A parse heap sized for a decoded boot script of `bytes` wire bytes.
fn heap_words_for(bytes: usize) -> usize {
    bytes.saturating_mul(24).max(4_096)
}

/// Walks the decoded `{script, {Name, Vsn}, Instructions}` term into a [`BootScript`].
fn parse_script(term: Eterm, atoms: &AtomTable, path: &Path) -> Result<BootScript> {
    let reader = ScriptReader { atoms, path };
    let arity = terms::tuple_arity(term)
        .map_err(|_| reader.malformed("the top-level term is not a tuple"))?;
    if arity != 3 {
        let detail = "the top-level term is not {script, {Name, Vsn}, Instructions}";
        return Err(reader.malformed(detail));
    }
    if reader.atom(reader.element(term, 0)?)? != "script" {
        return Err(reader.malformed("the top-level tuple does not begin with the atom `script`"));
    }
    let header = reader.element(term, 1)?;
    let name = reader.string(reader.element(header, 0)?)?;
    let vsn = reader.string(reader.element(header, 1)?)?;
    let list = terms::list_elements(reader.element(term, 2)?)
        .map_err(|_| reader.malformed("the instruction list is not a proper list"))?;
    let mut instructions = Vec::with_capacity(list.len());
    for instruction in list {
        instructions.push(reader.instruction(instruction)?);
    }
    Ok(BootScript { name, vsn, instructions })
}

/// The typed walk over one decoded boot script: shared atom resolution plus the shape
/// errors (whose messages name the offending file).
struct ScriptReader<'a> {
    atoms: &'a AtomTable,
    path: &'a Path,
}

impl ScriptReader<'_> {
    /// A malformed-image error naming `path`.
    fn malformed(&self, detail: impl Display) -> BeamError {
        error::message(format!("{} is not a boot script ({detail})", self.path.display()))
    }

    /// A capability-gap error for a directive this runtime does not model.
    fn unsupported(&self, directive: &str) -> BeamError {
        error::unsupported(&format!(
            "boot-script directive `{directive}` in {}",
            self.path.display()
        ))
    }

    /// Element `index` of the tuple `term`, or a malformed-image error.
    fn element(&self, term: Eterm, index: usize) -> Result<Eterm> {
        terms::tuple_element(term, index)
            .map_err(|_| self.malformed(format!("tuple element {index} is missing")))
    }

    /// Resolves an atom term to its name.
    fn atom(&self, term: Eterm) -> Result<String> {
        if !is_atom(term) {
            return Err(self.malformed("expected an atom"));
        }
        self.atoms
            .name(atom_val(term))
            .map(str::to_string)
            .ok_or_else(|| self.malformed("an atom name is not in the script's atom table"))
    }

    /// Resolves an Erlang string term (a proper list of character codes) to a `String`.
    fn string(&self, term: Eterm) -> Result<String> {
        if is_nil(term) {
            return Ok(String::new());
        }
        let elements = terms::list_elements(term)
            .map_err(|_| self.malformed("expected a string (a proper list of characters)"))?;
        let mut text = String::with_capacity(elements.len());
        for element in elements {
            if !is_small(element) {
                return Err(self.malformed("a string element is not a small integer"));
            }
            let code = signed_value(element);
            let ch = u32::try_from(code)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| self.malformed(format!("{code} is not a Unicode code point")))?;
            text.push(ch);
        }
        Ok(text)
    }

    /// Resolves a list of atoms to their names.
    fn atom_list(&self, term: Eterm) -> Result<Vec<String>> {
        let elements = terms::list_elements(term)
            .map_err(|_| self.malformed("expected a proper list of atoms"))?;
        elements.into_iter().map(|element| self.atom(element)).collect()
    }

    /// Resolves a list of strings to `String`s.
    fn string_list(&self, term: Eterm) -> Result<Vec<String>> {
        let elements = terms::list_elements(term)
            .map_err(|_| self.malformed("expected a proper list of strings"))?;
        elements.into_iter().map(|element| self.string(element)).collect()
    }

    /// Parses one `{Module, Function, Args}` triple.
    fn mfa(&self, term: Eterm) -> Result<Mfa> {
        let arity = terms::tuple_arity(term)
            .map_err(|_| self.malformed("an MFA is not a tuple"))?;
        if arity != 3 {
            return Err(self.malformed("an MFA is not a {Module, Function, Args} triple"));
        }
        let args = self.element(term, 2)?;
        let Value::List(args) = self.value(args)? else {
            return Err(self.malformed("an MFA's arguments are not a list"));
        };
        Ok(Mfa {
            module: self.atom(self.element(term, 0)?)?,
            function: self.atom(self.element(term, 1)?)?,
            args,
        })
    }

    /// Parses one argument value (recursively).
    fn value(&self, term: Eterm) -> Result<Value> {
        if is_atom(term) {
            return Ok(Value::Atom(self.atom(term)?));
        }
        if is_small(term) {
            return Ok(Value::Int(signed_value(term)));
        }
        if is_nil(term) {
            return Ok(Value::List(Vec::new()));
        }
        if is_list(term) {
            let elements = terms::list_elements(term)
                .map_err(|_| self.malformed("an improper list in a boot-script argument"))?;
            let values: Result<Vec<Value>> =
                elements.into_iter().map(|element| self.value(element)).collect();
            let values = values?;
            if let Some(text) = chars_of(&values) {
                return Ok(Value::Str(text));
            }
            return Ok(Value::List(values));
        }
        if let Ok(arity) = terms::tuple_arity(term) {
            let mut values = Vec::with_capacity(arity);
            for index in 0..arity {
                values.push(self.value(self.element(term, index)?)?);
            }
            return Ok(Value::Tuple(values));
        }
        // A pid, fun, float, big integer, binary, map, ref, … — not in the literal set.
        Ok(Value::Other)
    }

    /// Parses one directive.
    fn instruction(&self, term: Eterm) -> Result<Instruction> {
        let arity = terms::tuple_arity(term)
            .map_err(|_| self.malformed("an instruction is not a tuple"))?;
        let tag = self.atom(self.element(term, 0)?)?;
        match (arity, tag.as_str()) {
            (2, "preLoaded") => Ok(Instruction::PreLoaded(self.atom_list(self.element(term, 1)?)?)),
            (2, "progress") => Ok(Instruction::Progress(self.atom(self.element(term, 1)?)?)),
            (2, "path") => Ok(Instruction::Path(self.string_list(self.element(term, 1)?)?)),
            (2, "primLoad") => Ok(Instruction::PrimLoad(self.atom_list(self.element(term, 1)?)?)),
            (2, "module") => Ok(Instruction::Module(self.atom(self.element(term, 1)?)?)),
            (1, "kernel_load_completed") => Ok(Instruction::KernelLoadCompleted),
            (2, "apply") => Ok(Instruction::Apply {
                mfa: self.mfa(self.element(term, 1)?)?,
            }),
            (3, "kernelProcess") => Ok(Instruction::KernelProcess {
                name: self.atom(self.element(term, 1)?)?,
                mfa: self.mfa(self.element(term, 2)?)?,
            }),
            _ => Err(self.unsupported(&tag)),
        }
    }
}

/// A list of [`Value::Int`] as a `String` when every element is a Unicode code point.
///
/// This is the Erlang string convention (`"abc"` is `[97,98,99]`); materializing a `Str`
/// and a `List` of the same integers produces an identical term, so the distinction only
/// affects how a [`BootScript`] reads.
fn chars_of(values: &[Value]) -> Option<String> {
    let mut text = String::with_capacity(values.len());
    for value in values {
        let Value::Int(code) = value else {
            return None;
        };
        text.push(char::from_u32(u32::try_from(*code).ok()?)?);
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use std::process::Command;

    /// The committed `start.boot` of this checkout (the oracle-identical real script).
    ///
    /// `CARGO_MANIFEST_DIR` is `<repo>/erts_rust/crates/beam_rt`, so the repository root
    /// (which holds `bootstrap/`) is three levels up.
    const START_BOOT: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../../bootstrap/bin/start.boot");

    /// Skips **loudly** (clear message, early return) when a system tool is missing — the
    /// runtime's code path and the oracle both need a real Erlang install.
    fn have_system_erlang() -> bool {
        match Command::new("erl").arg("-noshell").arg("-eval").arg("halt(0).").output() {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                eprintln!("SKIP: `erl` exited with {}", output.status);
                false
            }
            Err(error) => {
                eprintln!("SKIP: cannot run `erl`: {error}");
                false
            }
        }
    }

    /// A runtime whose code path includes the system directories, or `None` when no Erlang
    /// install is available (the caller then skips **loudly**).
    fn system_runtime() -> Option<Runtime> {
        if !have_system_erlang() {
            return None;
        }
        match Runtime::with_defaults() {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                eprintln!("SKIP: no system Erlang install ({error})");
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // The real start.boot
    // -----------------------------------------------------------------------

    #[test]
    fn reads_the_real_start_boot_into_the_oracle_instruction_sequence() {
        // Oracle (`bootstrap/bin/start.boot`, OTP 29/erts-17.1):
        //   erl -noshell -eval '{ok,B}=file:read_file("bootstrap/bin/start.boot"),
        //       io:format("~p~n",[binary_to_term(B)]), halt().'
        let path = Path::new(START_BOOT);
        if !path.is_file() {
            eprintln!("SKIP: {START_BOOT} is absent");
            return;
        }
        let script = read(path).expect("the real start.boot parses");
        assert_eq!(script.name, "Erlang/OTP");
        assert_eq!(script.vsn, "30");
        assert_eq!(script.instructions.len(), 22, "the oracle has 22 instructions");

        let pre_loaded = |name: &str| Value::Atom(name.to_string());
        let mut expected = vec![
            Instruction::PreLoaded(
                [
                    "atomics",
                    "counters",
                    "erl_init",
                    "erl_prim_loader",
                    "erl_tracer",
                    "erlang",
                    "erts_code_purger",
                    "erts_dirty_process_signal_handler",
                    "erts_internal",
                    "erts_literal_area_collector",
                    "erts_trace_cleaner",
                    "init",
                    "persistent_term",
                    "prim_buffer",
                    "prim_eval",
                    "prim_file",
                    "prim_inet",
                    "prim_net",
                    "prim_socket",
                    "prim_zip",
                    "socket_registry",
                    "zlib",
                ]
                .iter()
                .map(|name| name.to_string())
                .collect(),
            ),
            Instruction::Progress("preloaded".to_string()),
            Instruction::Path(vec![
                "$ROOT/lib/kernel/ebin".to_string(),
                "$ROOT/lib/stdlib/ebin".to_string(),
            ]),
            Instruction::PrimLoad(
                [
                    "error_handler",
                    "application",
                    "application_controller",
                    "application_master",
                    "code",
                    "code_server",
                    "erl_eval",
                    "erl_lint",
                    "erl_parse",
                    "error_logger",
                    "ets",
                    "file",
                    "filename",
                    "file_server",
                    "file_io_server",
                    "gen",
                    "gen_event",
                    "gen_server",
                    "heart",
                    "kernel",
                    "logger",
                    "logger_filters",
                    "logger_server",
                    "logger_backend",
                    "logger_config",
                    "logger_simple_h",
                    "lists",
                    "proc_lib",
                    "supervisor",
                ]
                .iter()
                .map(|name| name.to_string())
                .collect(),
            ),
            Instruction::KernelLoadCompleted,
            Instruction::Progress("kernel_load_completed".to_string()),
            Instruction::Path(vec!["$ROOT/lib/kernel/ebin".to_string()]),
        ];
        // The second `primLoad` batch (kernel) and the third (stdlib), checked by size and
        // by their first/last names rather than by reproducing 173 names here. The last
        // `preLoaded` entry (already asserted above) pins the atom order.
        let Instruction::PrimLoad(kernel_batch) = &script.instructions[7] else {
            panic!("instruction 7 must be primLoad, got {:?}", script.instructions[7]);
        };
        assert_eq!(kernel_batch.len(), 86, "the kernel batch has 86 modules");
        assert_eq!(kernel_batch.first().map(String::as_str), Some("application_starter"));
        assert_eq!(kernel_batch.last().map(String::as_str), Some("wrap_log_reader"));
        let Instruction::PrimLoad(stdlib_batch) = &script.instructions[9] else {
            panic!("instruction 9 must be primLoad, got {:?}", script.instructions[9]);
        };
        assert_eq!(stdlib_batch.len(), 87, "the stdlib batch has 87 modules");
        assert_eq!(stdlib_batch.first().map(String::as_str), Some("argparse"));
        assert_eq!(stdlib_batch.last().map(String::as_str), Some("zstd"));

        // Every instruction before the second `primLoad` batch is pinned name-for-name.
        expected.extend([
            Instruction::PrimLoad(kernel_batch.clone()),
            Instruction::Path(vec!["$ROOT/lib/stdlib/ebin".to_string()]),
            Instruction::PrimLoad(stdlib_batch.clone()),
            Instruction::Progress("modules_loaded".to_string()),
            Instruction::Path(vec![
                "$ROOT/lib/kernel/ebin".to_string(),
                "$ROOT/lib/stdlib/ebin".to_string(),
            ]),
        ]);
        // The three kernel processes, in order.
        expected.push(Instruction::KernelProcess {
            name: "heart".to_string(),
            mfa: Mfa {
                module: "heart".to_string(),
                function: "start".to_string(),
                args: Vec::new(),
            },
        });
        expected.push(Instruction::KernelProcess {
            name: "logger".to_string(),
            mfa: Mfa {
                module: "logger_server".to_string(),
                function: "start_link".to_string(),
                args: Vec::new(),
            },
        });
        // Then the tail: `progress`, the two `apply`s, the erlangrc apply and `progress`.
        expected.extend([
            Instruction::Progress("init_kernel_started".to_string()),
            Instruction::Apply {
                mfa: Mfa {
                    module: "application".to_string(),
                    function: "start_boot".to_string(),
                    args: vec![pre_loaded("kernel"), pre_loaded("permanent")],
                },
            },
        ]);
        assert_eq!(
            expected[..14],
            script.instructions[..14],
            "instructions 0..13 must match the oracle exactly"
        );
        // Instruction 14 is the `application_controller` kernel process whose single
        // argument embeds the *loaded* `kernel.app` property list — large and
        // release-dependent, so pin its shape rather than reproducing every property.
        let Instruction::KernelProcess { name, mfa } = &script.instructions[14] else {
            panic!(
                "instruction 14 must be a kernelProcess, got {:?}",
                script.instructions[14]
            );
        };
        assert_eq!(name, "application_controller");
        assert_eq!(mfa.module, "application_controller");
        assert_eq!(mfa.function, "start");
        match &mfa.args[..] {
            [Value::Tuple(application)] => {
                assert_eq!(application[0], Value::Atom("application".to_string()));
                assert_eq!(application[1], Value::Atom("kernel".to_string()));
                let Value::List(props) = &application[2] else {
                    panic!("the kernel app's third element must be a property list");
                };
                assert_eq!(
                    props[..3],
                    [
                        Value::Tuple(vec![
                            Value::Atom("description".to_string()),
                            Value::Str("ERTS  CXC 138 10".to_string()),
                        ]),
                        Value::Tuple(vec![
                            Value::Atom("vsn".to_string()),
                            Value::Str("11.0.4".to_string()),
                        ]),
                        Value::Tuple(vec![Value::Atom("id".to_string()), Value::List(Vec::new())]),
                    ],
                    "the kernel property list opens with description/vsn/id"
                );
                assert!(
                    props
                        .iter()
                        .any(|p| *p == Value::Tuple(vec![
                            Value::Atom("mod".to_string()),
                            Value::Tuple(vec![
                                Value::Atom("kernel".to_string()),
                                Value::List(Vec::new()),
                            ]),
                        ])),
                    "the kernel property list carries its `mod` entry"
                );
            }
            other => panic!("application_controller:start/1 takes one 3-tuple: {other:?}"),
        }
        assert_eq!(script.instructions[15], expected[14]);
        // Instruction 16 is the big `application:load/1` whose single argument is the
        // stdlib `{application, stdlib, Props}` tuple; pin its shape without reproducing
        // the whole property list.
        let Instruction::Apply { mfa } = &script.instructions[16] else {
            panic!("instruction 16 must be apply, got {:?}", script.instructions[16]);
        };
        assert_eq!(mfa.module, "application");
        assert_eq!(mfa.function, "load");
        match &mfa.args[..] {
            [Value::Tuple(application)] => {
                assert_eq!(application[0], Value::Atom("application".to_string()));
                assert_eq!(application[1], Value::Atom("stdlib".to_string()));
                assert!(matches!(application[2], Value::List(_)), "a property list");
            }
            other => {
                panic!("application:load/1 takes one {{application, stdlib, Props}}: {other:?}")
            }
        }
        assert_eq!(
            script.instructions.len(),
            22,
            "the oracle's start.boot carries 22 instructions"
        );
        // The tail after `application:load/1`, in oracle order.
        assert_eq!(
            script.instructions[17..],
            [
                Instruction::Progress("applications_loaded".to_string()),
                Instruction::Apply {
                    mfa: Mfa {
                        module: "application".to_string(),
                        function: "start_boot".to_string(),
                        args: vec![pre_loaded("kernel"), pre_loaded("permanent")],
                    },
                },
                Instruction::Apply {
                    mfa: Mfa {
                        module: "application".to_string(),
                        function: "start_boot".to_string(),
                        args: vec![pre_loaded("stdlib"), pre_loaded("permanent")],
                    },
                },
                Instruction::Apply {
                    mfa: Mfa {
                        module: "c".to_string(),
                        function: "erlangrc".to_string(),
                        args: Vec::new(),
                    },
                },
                Instruction::Progress("started".to_string()),
            ]
        );
    }

    #[test]
    fn the_real_start_boot_prefix_loads_its_prim_load_batches() {
        // The bounded check: executing only the instructions *before* the first `apply`
        // must succeed and must have loaded every module the script names.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let path = Path::new(START_BOOT);
        if !path.is_file() {
            eprintln!("SKIP: {START_BOOT} is absent");
            return;
        }
        let script = read(path).expect("the real start.boot parses");
        let first_apply = script
            .instructions
            .iter()
            .position(|instruction| matches!(instruction, Instruction::Apply { .. }))
            .expect("the script applies something");
        let mut prefix = script.clone();
        prefix.instructions.truncate(first_apply);
        execute(&mut runtime, &prefix).expect("the primLoad prefix executes");
        for module in [
            "application",
            "application_controller",
            "code_server",
            "lists",
            "supervisor",
            "application_starter",
            "gen_server",
            "argparse",
            "zstd",
        ] {
            assert!(
                runtime.module(module).is_some(),
                "{module} must have been loaded by the script"
            );
        }
    }

    #[test]
    fn the_real_start_boot_stops_honestly_at_the_first_unbootable_directive() {
        // The full script loads every `primLoad` batch and then stops with a typed error at
        // the first directive M1 cannot run. It **must not** panic or hang: `execute` drives
        // the engine directly, so a blocking receive is the engine's typed `error::blocked()`.
        // The test is bounded by construction: it executes the instructions one at a time and
        // stops at the first failure, so a hang would have to be in a single `Machine` run.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let path = Path::new(START_BOOT);
        if !path.is_file() {
            eprintln!("SKIP: {START_BOOT} is absent");
            return;
        }
        let script = read(path).expect("the real start.boot parses");
        let mut failure = None;
        for (index, instruction) in script.instructions.iter().enumerate() {
            let single = BootScript {
                name: script.name.clone(),
                vsn: script.vsn.clone(),
                instructions: vec![instruction.clone()],
            };
            if let Err(error) = execute(&mut runtime, &single) {
                failure = Some((index, error));
                break;
            }
        }
        let (index, error) = failure.expect("the real start.boot cannot fully boot in Milestone 1");
        // A typed error, never a panic: every failure the runtime reports is a
        // `BeamError::Raise` (a load `undef`, a BIF raise, an engine capability gap).
        assert!(
            matches!(error, BeamError::Raise(_)),
            "instruction {index} ({:?}) must fail with a typed BeamError: {error}",
            script.instructions[index]
        );
        assert!(
            index >= 12,
            "the stop must be after the primLoad batches, found instruction {index}"
        );
        // The `primLoad` batches really loaded before the stop.
        for module in ["lists", "gen_server", "application_starter", "argparse", "zstd"] {
            assert!(
                runtime.module(module).is_some(),
                "{module} must have been loaded before the stop"
            );
        }
        eprintln!(
            "note: the real start.boot stops at instruction {index} ({:?}): {error}",
            script.instructions[index]
        );
    }

    // -----------------------------------------------------------------------
    // A synthetic script, executed end to end
    // -----------------------------------------------------------------------

    /// The fixture module the synthetic boot script applies.
    const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/src/boot/bootscript.rs` (never committed).
-module(bootscript_fixture).
-export([mark/0, marked/0]).
%% The side effect the boot script's `apply` observes: a process-dictionary entry on the
%% process the script runs as (the runtime's own `init` process).
mark() -> erlang:put(bootscript_mark, seen).
%% Reads that entry back.
marked() -> erlang:get(bootscript_mark).
"#;

    /// A compiled fixture in its own scratch directory.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn cleanup(self) {
            match std::fs::remove_dir_all(&self.dir) {
                Ok(()) => {}
                Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
            }
        }
    }

    /// Compiles the fixture and writes the synthetic `.boot`, or `None` when the toolchain
    /// is unavailable (the caller returns early — loudly).
    fn build_synthetic(label: &str) -> Option<Fixture> {
        let name = format!("beam_rt_bootscript_{label}_{}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            eprintln!("SKIP bootscript: cannot create {}: {error}", dir.display());
            return None;
        }
        let source = dir.join("bootscript_fixture.erl");
        if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
            eprintln!("SKIP bootscript: cannot write {}: {error}", source.display());
            return None;
        }
        let output = match Command::new("erlc").arg("-o").arg(&dir).arg(&source).output() {
            Ok(output) => output,
            Err(error) => {
                eprintln!("SKIP bootscript: cannot run `erlc`: {error}");
                return None;
            }
        };
        if !output.status.success() {
            eprintln!(
                "SKIP bootscript: `erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
        let beam = dir.join("bootscript_fixture.beam");
        if !beam.is_file() {
            eprintln!("SKIP bootscript: `erlc` produced no {}", beam.display());
            return None;
        }
        // The synthetic script: the preloaded set, a code-path entry, one module load and
        // the observed `apply`, in the exact shape the real `init` reads.
        let boot = dir.join("synthetic.boot");
        let dir_text = dir.display();
        let program = format!(
            "ok = file:write_file(\"{boot}\", term_to_binary({{script,{{\"t\",\"1\"}},[\
             {{preLoaded,[erlang]}},\
             {{progress,preloaded}},\
             {{path,[\"{dir_text}\"]}},\
             {{module,lists}},\
             {{apply,{{bootscript_fixture,mark,[]}}}}]}})), halt(0).",
            boot = boot.display(),
        );
        let output = match Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&program)
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                eprintln!("SKIP bootscript: cannot run `erl`: {error}");
                return None;
            }
        };
        if !output.status.success() {
            eprintln!(
                "SKIP bootscript: `erl` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
        let boot_path = dir.join("synthetic.boot");
        if !boot_path.is_file() {
            eprintln!("SKIP bootscript: `erl` produced no {}", boot_path.display());
            return None;
        }
        Some(Fixture { dir })
    }

    #[test]
    fn a_synthetic_script_executes_end_to_end() {
        let Some(fixture) = build_synthetic("e2e") else {
            return;
        };
        let boot = fixture.dir.join("synthetic.boot");
        // Parse the script we just wrote and check its typed form.
        let script = read(&boot).expect("the synthetic script parses");
        assert_eq!(script.name, "t");
        assert_eq!(script.vsn, "1");
        assert_eq!(
            script.instructions,
            vec![
                Instruction::PreLoaded(vec!["erlang".to_string()]),
                Instruction::Progress("preloaded".to_string()),
                Instruction::Path(vec![fixture.dir.display().to_string()]),
                Instruction::Module("lists".to_string()),
                Instruction::Apply {
                    mfa: Mfa {
                        module: "bootscript_fixture".to_string(),
                        function: "mark".to_string(),
                        args: Vec::new(),
                    },
                },
            ]
        );

        // Execute it against a runtime whose code path holds the fixture.
        let mut config = RuntimeConfig::default();
        config.code_paths.push(fixture.dir.clone());
        let mut runtime = Runtime::new(config).expect("the default runtime");
        runtime
            .boot_from_script(&boot)
            .expect("the synthetic script executes end to end");

        // The `apply` step really ran: the module it called is loaded, and the effect it had
        // on the running process's dictionary is observable through the same runtime.
        assert!(runtime.module("bootscript_fixture").is_some(), "the applied module loaded");
        assert!(runtime.module("lists").is_some(), "the `module` directive loaded lists");
        let marked = runtime
            .eval("bootscript_fixture:marked()")
            .expect("bootscript_fixture:marked/0");
        assert_eq!(
            runtime.atom_name_of(marked),
            Some("seen"),
            "the apply step's side effect must be observable"
        );
        fixture.cleanup();
    }

    // -----------------------------------------------------------------------
    // Malformed input
    // -----------------------------------------------------------------------

    #[test]
    fn a_malformed_boot_script_is_a_typed_error() {
        let name = format!("beam_rt_bootscript_bad_{}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).expect("scratch dir");

        // An empty file: no version byte.
        let empty = dir.join("empty.boot");
        std::fs::write(&empty, b"").expect("write");
        assert!(read(&empty).is_err(), "an empty image must not parse");

        // Random bytes: an unknown tag / bad version.
        let random = dir.join("random.boot");
        std::fs::write(&random, [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9]).expect("write");
        assert!(read(&random).is_err(), "random bytes must not parse");

        // A valid ETF image of a *wrong-shaped* term: `131` (VERSION_MAGIC) + `97`
        // (SMALL_INTEGER_EXT) + `1`.
        let wrong = dir.join("wrong.boot");
        std::fs::write(&wrong, [131u8, 97, 1]).expect("write");
        let error = read(&wrong).expect_err("a non-script term must be rejected");
        assert!(
            error::unsupported_reason(&error).is_none(),
            "a malformed image is not a capability gap: {error}"
        );

        // A truncated real script: the decoder stops early.
        let path = Path::new(START_BOOT);
        if path.is_file() {
            let bytes = std::fs::read(path).expect("read start.boot");
            let truncated = dir.join("truncated.boot");
            std::fs::write(&truncated, &bytes[..bytes.len() / 2]).expect("write");
            assert!(read(&truncated).is_err(), "a truncated image must not parse");
        } else {
            eprintln!("note: {START_BOOT} is absent; the truncation case is skipped");
        }

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
