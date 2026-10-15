// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

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
//! The real `start.boot` (OTP 30/erts-17.1, 22 instructions) uses exactly these eight
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
//! | `{kernelProcess, Name, {M,F,A}}` | spawn a kernel process (it registers `Name` itself) |
//! | `{apply, {M,F,A}}` | call `M:F(A...)` |
//!
//! An instruction outside this set is a typed **capability gap** ([`crate::error::unsupported`]),
//! never a silent skip; a malformed image is a typed [`BeamError`](beam_common::BeamError).
//!
//! # Execution stops at the first directive that cannot run
//!
//! [`execute`] walks the instructions in order and returns the first error it meets —
//! loading a module that is not on the code path, spawning a process whose function is not
//! exported, or applying an MFA the runtime cannot run. This is the honest behaviour: the
//! real `start.boot` loads every `primLoad` batch, runs all three `kernelProcess`
//! directives and completes all 22 of them to its final `{progress,started}`, and a script
//! that needs machinery this runtime does not have would stop at the first such directive
//! with its typed error rather than skipping it (see `src/boot/CONTEXT.md`).
//!
//! # The directives that run processes advance the scheduler
//!
//! The two directives that involve a process — [`Instruction::KernelProcess`] and
//! [`Instruction::Apply`] — are what make `init`'s sequence more than a list of loads, and
//! this executor really drives the runtime's **scheduler**
//! ([`Runtime::pump_scheduler_boot`](crate::runtime::Runtime::pump_scheduler_boot)) for them:
//!
//! * a `{kernelProcess, …}` spawns its process and then drives a **bounded burst** of
//!   scheduler slices, so a long-lived kernel server (`heart`, `logger_server`,
//!   `application_controller`) can take its first steps — typically reaching its `receive`
//!   and **registering its own name** — before a later directive talks to it. The executor
//!   never registers that name: `init` does not either (see
//!   [`drive::start_kernel_process`]). A process that crashes during the burst is **not**
//!   this directive's error: the failure surfaces at the next directive that depends on it;
//! * an `{apply, {M,F,A}}` runs `M:F(A...)` **as this runtime's own process** (the identity
//!   [`Runtime::eval`](crate::runtime::Runtime::eval) uses, so its process-dictionary side
//!   effects stay observable), but through the engine's blocking-aware entries: when the
//!   call suspends in a `receive` waiting for a boot-spawned process, the scheduler is
//!   driven and the call is resumed, exactly as
//!   [`Runtime::run_scheduled`](crate::runtime::Runtime::run_scheduled) resumes a blocked
//!   workload. A call the runtime's own non-blocking dispatch chain serves — a NIF, the
//!   runtime-native floor, a registered `beam_bifs` BIF — is *not* a module export and is
//!   dispatched there instead (see [`apply_boot_call`](super::drive::apply_boot_call)).
//!
//! Every drive is **bounded**: a per-`pump_scheduler_boot` slice budget between deadline checks
//! and one wall-clock [`DRIVE_DEADLINE`](super::drive::DRIVE_DEADLINE) per [`execute`], so the
//! boot can never hang. When
//! the deadline expires the drive reports a typed [`error::message`] rather than waiting
//! forever; when a blocked apply has nothing left that could wake it, the engine's own
//! typed [`beam_engine::blocked`] is reported. A drive **sleeps only on an imminent timer
//! deadline** (within one pump slice), so a boot-armed timer a second away costs it nothing.
use std::collections::HashMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::time::Instant;

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{
    atom_val, is_atom, is_list, is_nil, is_small, make_small, signed_value, AtomTable, Eterm,
};

use crate::context::RuntimeContext;
use crate::error;
use crate::runtime::Runtime;

use super::drive;

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
    /// `{kernelProcess, Name, {M,F,A}}` — spawn a kernel process.
    ///
    /// `Name` is the name the process registers **itself** under, exactly as `init` treats
    /// it; the executor never registers it (see [`super::drive::start_kernel_process`]).
    KernelProcess {
        /// The name the process registers under — read back with `erlang:whereis/1`.
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
/// * a `{kernelProcess, …}` spawns its process and then drives a bounded scheduler burst,
///   which ends as soon as the process has registered `name` itself (the executor never
///   registers it — `init` does not either); a `{apply, …}` runs `M:F(A...)` as this
///   runtime's own process and drives the scheduler whenever it suspends in a `receive`;
/// * both drives are bounded by one wall-clock [`DRIVE_DEADLINE`](super::drive::DRIVE_DEADLINE)
///   for the whole pass, so the boot never hangs — an exhausted deadline is a typed
///   [`error::message`], and a blocked apply with nothing left to wake it is the engine's
///   typed [`beam_engine::blocked`].
///
/// # Errors
///
/// The first directive's error (see above), propagated unchanged.
pub fn execute(runtime: &mut Runtime, script: &BootScript) -> Result<()> {
    execute_reporting(runtime, script).map_err(|stop| stop.error)
}

/// Where and why a boot script stopped — the [`execute_reporting`] report.
///
/// [`execute`] flattens a stop into its [`BeamError`]; this carries the **position** of that
/// error so a caller can name the exact directive a boot reached (the runtime's boot
/// observability, a differential case). All fields are the state at the moment of the stop.
#[derive(Debug)]
pub struct BootStop {
    /// The 0-based index of the failing directive, or `None` when the runtime failed to boot
    /// before the first directive ran.
    pub index: Option<usize>,
    /// The script's instruction count.
    pub total: usize,
    /// The failing directive, or `None` when no directive ran.
    pub instruction: Option<Instruction>,
    /// The error the failing directive reported.
    pub error: BeamError,
}

impl BootStop {
    /// A one-line, human-readable summary of the stop: the instruction position, the failing
    /// directive (an `M:F/A` or a `{kernelProcess,Name,…}`) and the error.
    ///
    /// The error is rendered by its [`Display`](std::fmt::Display), so a **structured** raise
    /// reason (an `exit noproc`, say) shows as `exit <structured reason>`. Use
    /// [`describe_with`](Self::describe_with) to render the real reason term through a runtime.
    #[must_use]
    pub fn describe(&self) -> String {
        self.render(&self.error.to_string())
    }

    /// [`describe`](Self::describe) with the error's **live reason term** rendered through
    /// `runtime` when it carries one.
    ///
    /// A structured raise reason is an opaque term, so [`describe`](Self::describe) can only
    /// show `exit <structured reason>`. This renders the real term — for a directive's unmet
    /// `gen_server:call` the `{noproc,{gen_server,call,[Name,…]}}` that names the missing
    /// server — so a report names the failing call. A non-structured error, or a reason the
    /// printer itself rejects, falls back to [`describe`](Self::describe) unchanged: never an
    /// invented reason.
    #[must_use]
    pub fn describe_with(&self, runtime: &mut Runtime) -> String {
        let Some(term) = crate::error::live_reason_term(&self.error) else {
            return self.describe();
        };
        match runtime.format_term_w(term, None) {
            // `format_term_w` ends the rendering with a newline; `render` supplies its own.
            Ok(text) => self.render(text.trim_end_matches('\n')),
            Err(_) => self.describe(),
        }
    }

    /// Formats the stop's position and directive around an already-rendered `reason`.
    fn render(&self, reason: &str) -> String {
        match (self.index, &self.instruction) {
            (Some(index), Some(instruction)) => format!(
                "boot script stopped at instruction {index}/{}: {}: {reason}",
                self.total,
                render_instruction(instruction)
            ),
            _ => format!("boot script failed before its first directive: {reason}"),
        }
    }
}

/// Renders one directive compactly for [`BootStop::describe`] (`M:F/A`, or the debug form).
fn render_instruction(instruction: &Instruction) -> String {
    match instruction {
        Instruction::Apply { mfa } => {
            format!("{}:{}/{}", mfa.module, mfa.function, mfa.args.len())
        }
        Instruction::KernelProcess { name, mfa } => format!(
            "{{kernelProcess,{name},{{{},{}/{}}}}}",
            mfa.module,
            mfa.function,
            mfa.args.len()
        ),
        other => format!("{other:?}"),
    }
}

/// Executes `script` against `runtime`, reporting the exact directive the boot stopped at.
///
/// This is [`execute`] with the failure's position preserved: it returns the same `Ok(())` on
/// a fully executed script, and on failure hands back a [`BootStop`] carrying the 0-based
/// instruction `index`, the script's `total`, the failing `instruction` and the `error`.
/// `execute` delegates to it (flattening the stop into its error), so the two can never drift.
///
/// The execution is identical to [`execute`]: the runtime is booted first (idempotent), one
/// wall-clock [`DRIVE_DEADLINE`](super::drive::DRIVE_DEADLINE) covers every scheduler drive of
/// the pass, and the walk stops at the first directive that cannot run.
///
/// # Errors
///
/// A [`BootStop`] for the first directive that cannot run (`instruction`/`index` set), or — if
/// the runtime cannot boot before the first directive — one with `index`/`instruction` unset.
pub fn execute_reporting(
    runtime: &mut Runtime,
    script: &BootScript,
) -> std::result::Result<(), BootStop> {
    // The script's `$ROOT` expansion and its module loads both need the system code path,
    // so the runtime is booted first (idempotent): a caller may hand `execute` a fresh
    // runtime, exactly as `Runtime::boot_from_script` does.
    let total = script.instructions.len();
    let before_first = |error: BeamError| BootStop {
        index: None,
        total,
        instruction: None,
        error,
    };
    runtime.boot().map_err(before_first)?;
    // One deadline for the whole pass: every scheduler drive below shares it, so the boot's
    // wall-clock bound does not multiply by the number of process directives.
    let deadline = Instant::now() + drive::DRIVE_DEADLINE;
    for (index, instruction) in script.instructions.iter().enumerate() {
        if let Err(error) = execute_instruction(runtime, instruction, deadline) {
            return Err(BootStop {
                index: Some(index),
                total,
                instruction: Some(instruction.clone()),
                error,
            });
        }
    }
    Ok(())
}

/// Executes one directive (the [`execute`] loop body).
///
/// `deadline` is the [`execute`] pass's shared scheduler-drive deadline (see
/// [`drive_scheduler`](super::drive::drive_scheduler)).
fn execute_instruction(
    runtime: &mut Runtime,
    instruction: &Instruction,
    deadline: Instant,
) -> Result<()> {
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
            // `init.erl` does **not** register `Name` itself — the process does (see
            // [`drive::start_kernel_process`]).
            drive::start_kernel_process(runtime, name, mfa, args, deadline)
        }
        Instruction::Apply { mfa } => {
            let args = boot_args(runtime, &mfa.args)?;
            drive::apply_boot_call(runtime, mfa, &args, deadline)
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

    /// The CLI argv the executor tests model: the `-boot FILE` invocation of this checkout,
    /// `beam_rt -noshell -boot bootstrap/bin/start` (the repository-root relative value a
    /// developer types). The value is **recorded** as `init` records it and is never resolved
    /// here — the executor is handed the parsed [`BootScript`] directly, read from
    /// [`START_BOOT`] — but it is part of the command line `init:get_arguments/0` reports,
    /// so the configuration is the real invocation's.
    fn boot_argv() -> Vec<String> {
        ["-noshell", "-boot", "bootstrap/bin/start"]
            .iter()
            .map(|argument| (*argument).to_string())
            .collect()
    }

    /// A runtime configured **exactly as the CLI configures a `-boot FILE` invocation**,
    /// or `None` when no Erlang install is available (the caller then skips **loudly**).
    ///
    /// The configuration is part of the measurement: a `-boot FILE` run boots the preloaded
    /// set and then drives the script, and the command line `init:get_arguments/0` reports
    /// is the *whole* invocation. Building the runtime through [`crate::cli::parse`] — the
    /// CLI's own parser, minus the `ERL_*FLAGS` merge a test must not depend on — is
    /// therefore what keeps the in-crate executor measuring the invocation the CLI really
    /// runs, rather than a configuration ([`RuntimeConfig::default`]'s empty argument list)
    /// the CLI cannot produce for it.
    fn system_runtime() -> Option<Runtime> {
        if !have_system_erlang() {
            return None;
        }
        let config = match crate::cli::parse(&boot_argv()) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("SKIP: cannot build the `-boot` invocation's configuration ({error})");
                return None;
            }
        };
        match Runtime::new(config) {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                eprintln!("SKIP: no system Erlang install ({error})");
                None
            }
        }
    }

    /// Whether `name` resolves to a **live** registered process in `runtime`.
    ///
    /// This is the observation `erlang:whereis/1` performs — the shared atom table plus the
    /// process table's name registry — read directly so the boot tests do not depend on the
    /// evaluator. A name the runtime's atom table has never interned is a miss.
    fn is_registered(runtime: &Runtime, name: &str) -> bool {
        runtime
            .atoms()
            .lookup(name)
            .and_then(|atom| runtime.processes().whereis(atom))
            .is_some()
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
    fn the_real_start_boot_executes_every_directive_to_progress_started() {
        // The full script loads every `primLoad` batch, spawns (and drives) the three kernel
        // processes, runs the four `apply` directives — the last of them, `{apply,{c,erlangrc,
        // []}}`, against `file_server_2`, which on this (`-boot FILE`) route is the booted
        // `kernel_sup`'s own `file_server` child because `Runtime::new` spawns the runtime's
        // native service **unclaimed** when a boot script is configured — and ends at the final
        // `{progress,started}`. It **must not** panic
        // or hang: every scheduler drive `execute` performs is bounded by its own deadline.
        //
        // Each instruction is executed in isolation here so a regression names the *exact*
        // directive it broke (the single-pass form is pinned by the companion test below).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let path = Path::new(START_BOOT);
        if !path.is_file() {
            eprintln!("SKIP: {START_BOOT} is absent");
            return;
        }
        let script = read(path).expect("the real start.boot parses");
        assert_eq!(script.instructions.len(), 22, "the oracle has 22 instructions");
        for (index, instruction) in script.instructions.iter().enumerate() {
            let single = BootScript {
                name: script.name.clone(),
                vsn: script.vsn.clone(),
                instructions: vec![instruction.clone()],
            };
            if let Err(error) = execute(&mut runtime, &single) {
                panic!("instruction {index} ({instruction:?}) must execute: {error}");
            }
        }
        // The last directive is the boot's completion marker.
        assert_eq!(
            script.instructions.last(),
            Some(&Instruction::Progress("started".to_string())),
            "the completed boot ends at the final `{{progress,started}}`"
        );
        // The `primLoad` batches really loaded.
        for module in ["lists", "gen_server", "application_starter", "argparse", "zstd"] {
            assert!(
                runtime.module(module).is_some(),
                "{module} must have been loaded by the script"
            );
        }
    }

    #[test]
    fn a_synthetic_unbootable_directive_reports_a_boot_stop() {
        // The `BootStop`/`execute_reporting` reporting machinery, pinned on a synthetic script
        // so the report's position and shape are deterministic (the real `start.boot` now
        // completes, so it has no `BootStop` to report — see `CONTEXT.md`). The second
        // instruction applies a non-existent module, so the walk stops at 0-based index 2 with
        // the script's instruction count, the failing directive and a typed error — never a panic.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let unbootable = Instruction::Apply {
            mfa: Mfa {
                module: "nonexistent_mod".to_string(),
                function: "no_such_fun".to_string(),
                args: Vec::new(),
            },
        };
        let script = BootScript {
            name: "synthetic".to_string(),
            vsn: "1".to_string(),
            instructions: vec![
                Instruction::PreLoaded(vec!["erlang".to_string()]),
                Instruction::Progress("preloaded".to_string()),
                unbootable.clone(),
                Instruction::Progress("started".to_string()),
            ],
        };
        let stop = execute_reporting(&mut runtime, &script)
            .expect_err("an unbootable directive must stop the script");
        assert_eq!(
            stop.index,
            Some(2),
            "the walk must stop at the unbootable directive: {}",
            stop.describe_with(&mut runtime)
        );
        assert_eq!(stop.total, 4, "the report counts every instruction");
        assert_eq!(
            stop.instruction,
            Some(unbootable),
            "the report names the failing directive"
        );
        // A typed error, never a panic: a missing module/export is a `BeamError::Raise`.
        assert!(
            matches!(stop.error, BeamError::Raise(_)),
            "the stop must be the directive's typed BeamError, got {}",
            stop.error
        );
        // The one-line report names the exact position and the failing `M:F/A`.
        let described = stop.describe_with(&mut runtime);
        assert!(
            described.contains("instruction 2/4")
                && described.contains("nonexistent_mod:no_such_fun/0"),
            "the report must name the position and the directive, got {described}"
        );
    }

    #[test]
    fn the_real_start_boot_completes_through_execute_reporting() {
        // The **whole** script in one pass: `execute_reporting` returns `Ok(())` — all 22
        // directives run, including the final `{apply,{c,erlangrc,[]}}` at instruction 20
        // (answered by the booted `kernel_sup`'s own `file_server` child) and the closing
        // `{progress,started}` — so there is no `BootStop` to report on this path.
        //
        // The three kernel processes must survive their short-lived directive processes'
        // exits (`heart`, `logger` and `application_controller`): the unlink in
        // `drive::start_kernel_process` keeps their servers alive, so `logger` and
        // `application_controller` are still registered after the whole script has run.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let path = Path::new(START_BOOT);
        if !path.is_file() {
            eprintln!("SKIP: {START_BOOT} is absent");
            return;
        }
        let script = read(path).expect("the real start.boot parses");
        execute_reporting(&mut runtime, &script)
            .expect("the real start.boot must boot all 22 directives to `{progress,started}`");
        assert_eq!(
            script.instructions.last(),
            Some(&Instruction::Progress("started".to_string())),
            "the completed boot ends at the final `{{progress,started}}`"
        );
        // The unlink in `drive::start_kernel_process` keeps the kernel servers alive past
        // their directive processes' `normal` exits. (`heart` returns without registering; the
        // `logger` server's survival is pinned by
        // `the_real_boot_registers_a_live_logger_process`, so `application_controller` is the
        // one to pin here.)
        let registered = |runtime: &Runtime, name: &str| {
            runtime
                .atoms()
                .lookup(name)
                .and_then(|atom| runtime.processes().whereis(atom))
                .is_some()
        };
        assert!(
            registered(&runtime, "application_controller"),
            "the application_controller server must survive its directive process"
        );
        // The boot's own file service. On **this** (`-boot FILE`) route `Runtime::new` spawns the
        // runtime's **native** `file_server_2` service *unclaimed*, so the name belongs to the
        // booted `kernel_sup`'s own `file_server` child (`lib/kernel/src/kernel.erl` starts it,
        // `file_server.erl:266-269` registers it) — which must still hold the name after the
        // script has consumed its replies.
        assert!(
            registered(&runtime, "file_server_2"),
            "the booted kernel's own file_server_2 child must still be registered after the boot"
        );
    }

    #[test]
    fn the_real_boot_registers_a_live_logger_process() {
        // Instruction 13's `{kernelProcess,logger,{logger_server,start_link,[]}}` leaves
        // `logger_server` alive and registered as `logger`, so after the boot
        // `erlang:whereis(logger)` resolves to a live pid and a second
        // `logger_server:start_link/0` reports `{error,{already_started,Pid}}`, exactly as under
        // real OTP.
        //
        // The child is entered at `proc_lib:init_p_do_apply/3`, whose `try … apply(M,F,A) …`
        // body `erlc` compiles to a **non-tail** `{call_ext,3,{extfunc,erlang,apply,3}}`. The
        // engine's `call_ext*` arms give `erlang:apply/3` C's `fixed_apply` treatment
        // (`beam_engine/src/interpreter/dispatch.rs::resolve_call_ext_apply`), so the callee is
        // entered **in-machine** as `Step::Enter` and its blocking `receive` suspends *this*
        // process through the ordinary `RunOutcome::Wait` path instead of folding into the
        // uncatchable typed `error::blocked()`. `gen_server:init_it` therefore reaches its
        // `register(logger, self())` and the directive's parked `proc_lib:sync_start/2`
        // (timeout `infinity`) resumes.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let path = Path::new(START_BOOT);
        if !path.is_file() {
            eprintln!("SKIP: {START_BOOT} is absent");
            return;
        }
        let script = read(path).expect("the real start.boot parses");
        execute_reporting(&mut runtime, &script)
            .expect("the real start.boot must boot all 22 directives to `{progress,started}`");
        assert!(
            is_registered(&runtime, "logger"),
            "the real boot must leave a live `logger` process registered (the oracle answers \
             `erlang:whereis(logger)` with a pid)"
        );
        // A second `logger_server:start_link/0` must report the live name. Compare the two
        // results through the runtime's own printer, so the exact pid never has to be pinned.
        let started = runtime
            .eval("logger_server:start_link()")
            .expect("logger_server:start_link/0 must return");
        let expected = runtime
            .eval("{error, {already_started, erlang:whereis(logger)}}")
            .expect("the expected `{error,{already_started,Pid}}`");
        assert_eq!(
            runtime
                .format_term_w(started, None)
                .expect("print the start_link result"),
            runtime
                .format_term_w(expected, None)
                .expect("print the expected result"),
            "a second logger_server:start_link/0 must report `{{error,{{already_started,Pid}}}}`"
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

    /// Compiles `sources` into a fresh scratch directory and writes the `.boot` image
    /// `program` builds there, or `None` — **loudly** — when the toolchain is unavailable.
    ///
    /// `sources` are `(file name, Erlang source)` pairs (each compiled with `erlc`); every
    /// listed module must produce a `.beam`, or the fixture is refused rather than used
    /// half-built. `program` is handed the scratch directory and the `.boot` path and
    /// returns the `erl` `-eval` expression that writes the script — exactly the shape the
    /// real `init` reads.
    fn build_fixture(
        label: &str,
        sources: &[(&str, &str)],
        program: impl FnOnce(&Path, &Path) -> String,
    ) -> Option<Fixture> {
        let name = format!("beam_rt_bootscript_{label}_{}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            eprintln!("SKIP bootscript: cannot create {}: {error}", dir.display());
            return None;
        }
        for (file, text) in sources {
            let source = dir.join(file);
            if let Err(error) = std::fs::write(&source, *text) {
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
            let beam = source.with_extension("beam");
            if !beam.is_file() {
                eprintln!("SKIP bootscript: `erlc` produced no {}", beam.display());
                return None;
            }
        }
        let boot = dir.join("synthetic.boot");
        let expression = program(&dir, &boot);
        let output = match Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&expression)
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
        if !boot.is_file() {
            eprintln!("SKIP bootscript: `erl` produced no {}", boot.display());
            return None;
        }
        Some(Fixture { dir })
    }

    /// The synthetic script of [`a_synthetic_script_executes_end_to_end`]: the preloaded
    /// set, a code-path entry, one module load and the observed `apply`.
    fn build_synthetic(label: &str) -> Option<Fixture> {
        build_fixture(label, &[("bootscript_fixture.erl", FIXTURE_ERL)], |dir, boot| {
            format!(
                "ok = file:write_file(\"{boot}\", term_to_binary({{script,{{\"t\",\"1\"}},[\
                 {{preLoaded,[erlang]}},\
                 {{progress,preloaded}},\
                 {{path,[\"{dir}\"]}},\
                 {{module,lists}},\
                 {{apply,{{bootscript_fixture,mark,[]}}}}]}})), halt(0).",
                boot = boot.display(),
                dir = dir.display(),
            )
        })
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
    // A boot-spawned process answering a blocked `apply`
    // -----------------------------------------------------------------------

    /// The fixture module of [`a_boot_spawned_process_answers_a_blocked_apply`].
    ///
    /// `responder/0` **registers its own name** (the `{kernelProcess, …}` name) exactly as a
    /// real kernel process does — the executor never registers it, so the registration has to
    /// come from the process.
    const WAITER_ERL: &str = r#"%% Generated by `beam_rt/src/boot/bootscript.rs` (never committed).
-module(bootscript_waiter).
-export([responder/0, ask/0, seen/0]).
%% The `{kernelProcess, …}` entry: register the directive's name **from the process**
%% (as `heart`/`logger_server`/`application_controller` each do), answer one ping, then settle
%% back into the `receive` so the process stays alive — and registered — afterwards.
responder() ->
    register(bootscript_kernel, self()),
    receive {ping, From} -> From ! {pong, hello} end.
%% The `{apply, …}` entry: ping the registered kernel process and block until it answers,
%% recording the reply in the dictionary of the process the script runs as.
ask() ->
    Pid = erlang:whereis(bootscript_kernel),
    Pid ! {ping, self()},
    receive {pong, Value} -> erlang:put(bootscript_seen, Value) end.
%% Reads the recorded reply back.
seen() -> erlang:get(bootscript_seen).
"#;

    /// The two process directives really **drive the scheduler**: a `{kernelProcess, …}`
    /// burst runs the spawned process up to its `receive` — after it has registered the
    /// directive's name **itself** — and a later `{apply, …}` that blocks on it is resumed by a
    /// scheduler drive and completes.
    ///
    /// This is the behaviour those two directives exist for. The script spawns
    /// `bootscript_kernel` (whose entry registers that name and then settles into a
    /// `receive`), then applies `ask/0`, which resolves the name with `erlang:whereis/1`, sends
    /// that process a ping and blocks in `receive` until it answers. The apply can only observe
    /// the reply if the burst ran the kernel process (so its `register/2` happened) *and* the
    /// apply's own `receive` was resumed after a scheduler drive — with a single-pass apply the
    /// send would have no one to answer it and the engine would report its typed `blocked`.
    /// The same test is therefore the regression pin for the executor **not** pre-registering
    /// the name: a pre-registration would make the fixture's `register/2` a `badarg`, killing
    /// the process before it ever reached its `receive`.
    #[test]
    fn a_boot_spawned_process_answers_a_blocked_apply() {
        let Some(fixture) = build_fixture(
            "wait",
            &[("bootscript_waiter.erl", WAITER_ERL)],
            |dir, boot| {
                format!(
                    "ok = file:write_file(\"{boot}\", term_to_binary({{script,{{\"t\",\"1\"}},[\
                     {{preLoaded,[erlang]}},\
                     {{progress,preloaded}},\
                     {{path,[\"{dir}\"]}},\
                     {{module,lists}},\
                     {{kernelProcess,bootscript_kernel,{{bootscript_waiter,responder,[]}}}},\
                     {{apply,{{bootscript_waiter,ask,[]}}}}]}})), halt(0).",
                    boot = boot.display(),
                    dir = dir.display(),
                )
            },
        ) else {
            return;
        };
        let boot = fixture.dir.join("synthetic.boot");
        let script = read(&boot).expect("the synthetic script parses");
        assert!(
            matches!(script.instructions[4], Instruction::KernelProcess { .. }),
            "instruction 4 must be the kernelProcess: {:?}",
            script.instructions[4]
        );
        assert!(
            matches!(script.instructions[5], Instruction::Apply { .. }),
            "instruction 5 must be the apply: {:?}",
            script.instructions[5]
        );

        let mut config = RuntimeConfig::default();
        config.code_paths.push(fixture.dir.clone());
        let mut runtime = Runtime::new(config).expect("the default runtime");
        runtime
            .boot_from_script(&boot)
            .expect("the boot-spawned process answers the blocked apply");

        let seen = runtime
            .eval("bootscript_waiter:seen()")
            .expect("bootscript_waiter:seen/0");
        assert_eq!(
            runtime.atom_name_of(seen),
            Some("hello"),
            "the apply must have been resumed by the kernel process's reply"
        );
        fixture.cleanup();
    }

    // -----------------------------------------------------------------------
    // The executor does NOT pre-register a kernel-process name
    // -----------------------------------------------------------------------

    /// The fixture module of [`a_kernel_process_registers_its_own_directive_name`].
    ///
    /// `serve/0` registers the *directive's* name **itself** and dies if that
    /// registration is refused — which is exactly what happens when the executor
    /// pre-registers the name (`register/2` of a live name is `error:badarg`). `probe/0`
    /// then resolves the name with `erlang:whereis/1`, so the script's later `{apply, …}`
    /// can only answer `registered` if the executor left the registration to the process.
    const SELFREG_ERL: &str = r#"%% Generated by `beam_rt/src/boot/bootscript.rs` (never committed).
-module(bootscript_selfreg).
-export([serve/0, probe/0, seen/0]).
serve() ->
    register(bootscript_self, self()),
    loop().
%% Stay alive (and registered) after answering, so the name really resolves afterwards.
loop() ->
    receive {ping, From} -> From ! {pong, registered}, loop() end.
probe() ->
    case erlang:whereis(bootscript_self) of
        Pid when is_pid(Pid) ->
            Pid ! {ping, self()},
            receive {pong, Value} -> erlang:put(bootscript_seen, Value) end;
        undefined ->
            erlang:put(bootscript_seen, not_registered)
    end.
seen() -> erlang:get(bootscript_seen).
"#;

    /// `init.erl`'s `{kernelProcess, Name, {M,F,A}}` directive **never registers `Name`**:
    /// `init` runs `apply(M,F,A)` (`erts/preloaded/src/init.erl:1520`) and the process
    /// registers itself (`heart.erl:187`, `logger_server.erl:55`,
    /// `application_controller.erl:531`). A pre-registration by the executor would make that
    /// self-registration `error:badarg` and kill the kernel process.
    ///
    /// This test pins the corrected contract end to end: the fixture's entry point registers
    /// the directive's name itself (and would die on a `badarg`), the burst runs it, and the
    /// script's later `{apply, …}` resolves that name with `erlang:whereis/1` and gets the
    /// **pid the process registered**.
    #[test]
    fn a_kernel_process_registers_its_own_directive_name() {
        let Some(fixture) = build_fixture(
            "selfreg",
            &[("bootscript_selfreg.erl", SELFREG_ERL)],
            |dir, boot| {
                format!(
                    "ok = file:write_file(\"{boot}\", term_to_binary({{script,{{\"t\",\"1\"}},[\
                     {{preLoaded,[erlang]}},\
                     {{progress,preloaded}},\
                     {{path,[\"{dir}\"]}},\
                     {{module,lists}},\
                     {{kernelProcess,bootscript_self,{{bootscript_selfreg,serve,[]}}}},\
                     {{apply,{{bootscript_selfreg,probe,[]}}}}]}})), halt(0).",
                    boot = boot.display(),
                    dir = dir.display(),
                )
            },
        ) else {
            return;
        };
        let boot = fixture.dir.join("synthetic.boot");
        let script = read(&boot).expect("the synthetic script parses");
        let Instruction::KernelProcess { name, .. } = &script.instructions[4] else {
            panic!("instruction 4 must be the kernelProcess: {:?}", script.instructions[4]);
        };
        assert_eq!(name, "bootscript_self", "the directive names the process");

        let mut config = RuntimeConfig::default();
        config.code_paths.push(fixture.dir.clone());
        let mut runtime = Runtime::new(config).expect("the default runtime");
        runtime
            .boot_from_script(&boot)
            .expect("the self-registering kernel process answers the blocked apply");
        fixture.cleanup();

        // The `{apply, …}` resolved the name with `erlang:whereis/1` and got a live pid, so
        // the answer is the fixture's `registered` — not `not_registered`, and (because the
        // fixture's own `register/2` would have been the `badarg` that kills it) not a dead
        // process either.
        let seen = runtime
            .eval("bootscript_selfreg:seen()")
            .expect("bootscript_selfreg:seen/0");
        assert_eq!(
            runtime.atom_name_of(seen),
            Some("registered"),
            "the apply must resolve the name the process registered itself"
        );

        // The name still resolves to the (still alive) self-registered process.
        let atom = runtime
            .atoms()
            .lookup("bootscript_self")
            .expect("the directive name is in the shared atom table");
        assert!(
            runtime.processes().whereis(atom).is_some(),
            "the self-registered name must still resolve after the burst"
        );
    }

    /// The fixture module of [`a_kernel_process_burst_waits_for_the_directive_to_return`].
    ///
    /// `serve/0` has exactly `application_controller:start/1`'s shape: it `spawn_link`s a
    /// server that registers the directive's name **first** and only *then* does its real
    /// setup, and `serve/0` blocks until that server acknowledges
    /// (`application_controller.erl:203-211`, the ack at `:549`). Only after the ack does the
    /// directive register its own second name (`bootscript_ack_done`) and settle into a
    /// `receive` — so that name is a **cross-process-visible marker** of "the directive's MFA
    /// has run to its end", which a burst that returned the instant the first name resolved
    /// can never have observed.
    const ACK_ERL: &str = r#"%% Generated by `beam_rt/src/boot/bootscript.rs` (never committed).
-module(bootscript_ack).
-export([serve/0, probe/0, seen/0]).
serve() ->
    Me = self(),
    Child = spawn_link(fun() -> server(Me) end),
    receive {ack, Child} -> ready() end.
ready() ->
    register(bootscript_ack_done, self()),
    loop().
server(Parent) ->
    register(bootscript_ack, self()),
    Parent ! {ack, self()},
    loop().
loop() ->
    receive {ping, From} -> From ! {pong, self()}, loop() end.
probe() ->
    Value =
        case {erlang:whereis(bootscript_ack), erlang:whereis(bootscript_ack_done)} of
            {Child, Done} when is_pid(Child), is_pid(Done) ->
                Child ! {ping, self()},
                receive {pong, _Pid} -> acked end;
            _ ->
                not_acked
        end,
    erlang:put(bootscript_seen, Value).
seen() -> erlang:get(bootscript_seen).
"#;

    /// The `{kernelProcess, …}` burst runs until the **directive's MFA has returned**, not
    /// merely until its name resolves.
    ///
    /// `application_controller:init/2` registers the name as its very first statement and runs
    /// `check_conf/0`, `ets:new(ac_tab, …)`, `make_appl/1` and `load/2` *afterwards*, only
    /// acking its directive (and calling `gen_server:enter_loop`) at the end
    /// (`lib/kernel/src/application_controller.erl:531-558`). Real `init` runs the directive
    /// **synchronously** (`init.erl:1520`), so the next directive only runs once the server is
    /// ready; a burst that returned at the first `whereis` resolution would leave it
    /// mid-`init` and unable to answer the next directive's call.
    ///
    /// This test pins the matching stop criterion end to end: the fixture's `serve/0` blocks
    /// until its `spawn_link`ed server acks and only then registers `bootscript_ack_done`, so
    /// the script's later `{apply, {bootscript_ack, probe, []}}` can answer `acked` (and get a
    /// reply from the server, proving the unlink left it in a working loop) only if the burst
    /// drove the directive to its very end.
    #[test]
    fn a_kernel_process_burst_waits_for_the_directive_to_return() {
        let Some(fixture) = build_fixture("ack", &[("bootscript_ack.erl", ACK_ERL)], |dir, boot| {
            format!(
                "ok = file:write_file(\"{boot}\", term_to_binary({{script,{{\"t\",\"1\"}},[\
                 {{preLoaded,[erlang]}},\
                 {{progress,preloaded}},\
                 {{path,[\"{dir}\"]}},\
                 {{module,lists}},\
                 {{kernelProcess,bootscript_ack,{{bootscript_ack,serve,[]}}}},\
                 {{apply,{{bootscript_ack,probe,[]}}}}]}})), halt(0).",
                boot = boot.display(),
                dir = dir.display(),
            )
        }) else {
            return;
        };
        let boot = fixture.dir.join("synthetic.boot");
        let script = read(&boot).expect("the synthetic script parses");
        let Instruction::KernelProcess { name, .. } = &script.instructions[4] else {
            panic!("instruction 4 must be the kernelProcess: {:?}", script.instructions[4]);
        };
        assert_eq!(name, "bootscript_ack", "the directive names the process");

        let mut config = RuntimeConfig::default();
        config.code_paths.push(fixture.dir.clone());
        let mut runtime = Runtime::new(config).expect("the default runtime");
        runtime
            .boot_from_script(&boot)
            .expect("the directive's ack is driven before the next directive runs");

        let seen = runtime
            .eval("bootscript_ack:seen()")
            .expect("bootscript_ack:seen/0");
        assert_eq!(
            runtime.atom_name_of(seen),
            Some("acked"),
            "the burst must drive the directive's MFA to its end (the ack), not stop at the \
             name resolution"
        );

        // Both the server and the directive are still registered and alive: the unlink landed
        // between the server's registration and the directive's exit, so neither died.
        for name in ["bootscript_ack", "bootscript_ack_done"] {
            let atom = runtime
                .atoms()
                .lookup(name)
                .unwrap_or_else(|| panic!("{name} is in the shared atom table"));
            assert!(
                runtime.processes().whereis(atom).is_some(),
                "{name} must still resolve after the burst"
            );
        }
        fixture.cleanup();
    }

    /// `Runtime::register_boot_process` keeps the process table's `name_taken` rule: a second
    /// registration of a live name is `badarg`, never a silent overwrite.
    ///
    /// This is *why* the executor must not pre-register a kernel-process name — the OTP
    /// kernel processes register themselves (`register(?AC, self())`,
    /// `gen_server:start_link({local, logger}, …)`), and a pre-registration would make that
    /// self-registration the same `badarg`. Verified in this runtime and in the live oracle:
    /// `register(nm,self()), register(nm,self())` raises `error:badarg`.
    #[test]
    fn registering_a_live_boot_name_is_badarg() {
        let mut runtime = Runtime::with_defaults().expect("the default runtime");
        let pid = runtime.self_pid();
        runtime
            .register_boot_process("boot_name_regression", pid)
            .expect("the first registration succeeds");
        let error = runtime
            .register_boot_process("boot_name_regression", pid)
            .expect_err("re-registering a live name must fail");
        assert!(
            matches!(error, BeamError::BadArg),
            "re-registering a live name must be badarg, got {error}"
        );
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
