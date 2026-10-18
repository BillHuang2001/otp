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
//! The **census probe harness**: drive one [`Instr`] with *valid operands* and a
//! *declared expectation*.
//!
//! `tests/census.rs` used to place every one of the 108 IR variants into one
//! synthetic module with a bare [`MinimalContext`] and degenerate operands, so an
//! instruction that merely *needed* a BIF hook, a stack frame or a tag atom in the
//! module's atom table reported a typed error and was counted as "claimed but
//! blocked". Those were probe artifacts, not engine gaps. This module is the
//! reusable replacement: a [`Probe`] is one row — the instruction under test, its
//! prologue/epilogue, per-probe module extras, hook/mailbox installation and a
//! declared [`Expect`] — and [`Probe::run`] reports whether the engine did exactly
//! what the row declared.
//!
//! # The generated module
//!
//! ```text
//! label 1              (ENTRY_LABEL)
//!   <setup…>           the probe's prologue
//!   <instr>            the instruction under test
//!   <epilogue…>        the probe's epilogue (a non-tail call's `deallocate`, a
//!                      register move into x(0), …)
//! label 2              (SUCCESS_LABEL)
//!   return             → the run returns x(0)
//! label 3              (FAIL_SINK_LABEL)
//!   move <fail mark> x(0)
//!   return             → the run returns the distinct FAIL mark atom
//! <extra blocks…>      the probe's own aux blocks (`aux_label(n)` bodies: a
//!                      callee leaf, a lambda body, a catch handler, …)
//! ```
//!
//! Two sinks, not one: the FAIL sink has its own label **and** returns a
//! distinguishable atom, so "the instruction branched to its declared `fail`"
//! ([`Observed::Branched`]) can never be confused with "the instruction fell
//! through and the run happened to return". The success sink stays a bare `return`
//! on the bottom frame (whose continuation is `Halt`), which finishes the run.
//!
//! `Function::labels` is derived from the **final** code (the interpreter builds one
//! module-wide label map, so a labelless `fail` operand would be a typed
//! `BeamError::BadArg` and a duplicated label would silently win last).
//!
//! # One canonical atom universe
//!
//! [`ProbeAtoms`] interns every atom a probe can need **once**, and
//! [`ProbeAtoms::table`] hands out a **clone** for a probe module. A
//! [`beam_term::AtomTable`] clone preserves every index, so the operand atoms a row
//! spells from the canonical ids (`Operand::Atom(atoms.named("badmatch"))`) resolve
//! by name inside the module — which is what the exception family's tag-atom
//! resolution reads. Nothing interns into a shared mutable table per row.
//!
//! # The hooks
//!
//! [`MinimalContext`] already carries every seam a probe needs
//! (`with_bif_hook`/`set_*`/`with_mailbox`/…), so no bespoke [`ExecutionContext`]
//! is implemented here: the [`Probe`] fields simply install what its row declares.
//!
//! # Ownership
//!
//! A [`ProbeRun`] **owns** the module, the machine and the context that produced its
//! [`Eterm`]s: every term in [`Observed`] points into that context's arena (or into
//! the module's literal area), so the run object must outlive any assertion on them.
//! Never copy an `Eterm` out of a `ProbeRun` that has been dropped.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::context::{
    ApplyHook, BifHook, ExternalResolver, LocalCallResolver, LocalFunResolver,
};
use beam_engine::{Machine, Mailbox, MinimalContext, RunOutcome};
use beam_loader::{
    Function, FunDescriptor, Import, Instr, Label, Module, Operand, RecordDefinition,
};
use beam_term::{is_small, make_atom, signed_value, Atom, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// The family-dispatch sentinel
// ---------------------------------------------------------------------------

/// The sentinel `beam_engine::families::dispatch` returns for an instruction that
/// **no** core arm and **no** instruction family claims.
///
/// It is `families::UNCLAIMED_PREFIX`, which is `pub(crate)` to the engine and so
/// cannot be imported here; the literal is reproduced verbatim (it is a stable,
/// documented diagnostic — see `src/families/mod.rs`). It is the **single**
/// definition for the test suite (the census re-exports it rather than repeating it).
pub const UNCLAIMED_PREFIX: &str = "widened opcode not yet executed";

/// The prefix of every **engine capability diagnostic** reason atom
/// ([`beam_engine::unsupported`]).
pub const ENGINE_DIAGNOSTIC_PREFIX: &str = "unsupported_opcode: ";

// ---------------------------------------------------------------------------
// The generated module's fixed labels
// ---------------------------------------------------------------------------

/// The probe module's entry label (the run's start).
pub const ENTRY_LABEL: Label = Label(1);
/// The success sink: a bare `return`, so the run yields `x(0)`.
pub const SUCCESS_LABEL: Label = Label(2);
/// The fail sink: writes [`ProbeAtoms::fail_mark`] into `x(0)` and returns.
pub const FAIL_SINK_LABEL: Label = Label(3);

/// The first label available to a probe's own aux blocks (a callee leaf, a lambda
/// body, a catch handler, …). Well above the three fixed labels and any label a
/// loader-produced module would use.
const AUX_LABEL_BASE: u32 = 30;

/// The `n`-th aux label (`aux_label(0)` = `Label(30)`), for a probe's own blocks.
///
/// A probe writes `Instr::Label(aux_label(0))` into [`Probe::extra_blocks`] and
/// refers to `aux_label(0)` from its operands.
pub fn aux_label(n: u32) -> Label {
    Label(AUX_LABEL_BASE + n)
}

// ---------------------------------------------------------------------------
// The canonical atom universe
// ---------------------------------------------------------------------------

/// Every atom a census probe may reference, interned **once**.
///
/// The list is deliberately closed: a probe that needs a name outside it interns it
/// into its own module's table through [`Probe::extra_atoms`], so the canonical
/// universe cannot silently grow into a per-row shared table.
const PROBE_ATOM_NAMES: &[&str] = &[
    // the probe module itself
    "probe",
    "f",
    "lambda",
    // the runtime names a BIF / atom-operand probe spells
    "erlang",
    "self",
    "abs",
    "unsafe",
    "integer",
    // the exception family's tag atoms + the class atom
    "badmatch",
    "case_clause",
    "if_clause",
    "try_clause",
    "badrecord",
    "error",
    "function_clause",
    "badarg",
    // a native-record definition and its fields
    "r",
    "a",
    "b",
    // the receive seam's "no marker to record" atom
    "undefined",
    // the fail sink's distinct mark
    "probe_fail",
    // record accessibility scope
    "external",
    "ok",
];

/// The canonical atom universe a probe module is built from.
///
/// [`ProbeAtoms::table`] yields a **clone** whose indices match the canonical ones,
/// so an operand built from [`ProbeAtoms::named`] resolves by name in the module.
pub struct ProbeAtoms {
    table: AtomTable,
}

impl ProbeAtoms {
    /// Intern the canonical universe.
    pub fn new() -> ProbeAtoms {
        let mut table = AtomTable::new();
        for name in PROBE_ATOM_NAMES {
            table.intern(name);
        }
        ProbeAtoms { table }
    }

    /// A module-ready clone of the canonical table (every index preserved).
    pub fn table(&self) -> AtomTable {
        self.table.clone()
    }

    /// The canonical id of `name`, or `None` when the universe does not hold it.
    pub fn get(&self, name: &str) -> Option<Atom> {
        self.table.lookup(name)
    }

    /// The canonical id of `name`; a name outside the universe is a **harness**
    /// error (the probe spelled an atom the universe does not hold).
    pub fn named(&self, name: &str) -> Atom {
        self.get(name)
            .unwrap_or_else(|| panic!("the canonical probe universe has no atom {name:?}"))
    }

    /// The probe module's name (`probe`).
    pub fn module(&self) -> Atom {
        self.named("probe")
    }

    /// The probe function's name (`f`).
    pub fn function(&self) -> Atom {
        self.named("f")
    }

    /// The atom the FAIL sink returns (`probe_fail`) — the distinct fail mark.
    pub fn fail_mark(&self) -> Eterm {
        make_atom(self.named("probe_fail"))
    }

    /// The receive seam's "nothing to record" atom (`undefined`).
    pub fn undefined(&self) -> Eterm {
        make_atom(self.named("undefined"))
    }

    /// The number of canonical atoms.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// Whether the universe is empty (never; it is the exhaustive match's companion).
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

impl Default for ProbeAtoms {
    fn default() -> ProbeAtoms {
        ProbeAtoms::new()
    }
}

// ---------------------------------------------------------------------------
// The mailbox
// ---------------------------------------------------------------------------

/// One recorded receive marker: the opaque marker term, the reference it was bound
/// to (if any) and the scan position it recorded.
struct Marker {
    term: Eterm,
    reference: Option<Eterm>,
    position: usize,
}

/// The mailbox's shared state (shared with the row so it can seed messages before
/// the run and inspect the queue after it).
#[derive(Default)]
struct MailboxState {
    /// The queued messages, in arrival order.
    messages: Vec<Eterm>,
    /// The current scan position (`receive_next` peeks here, `receive_skip`
    /// advances it).
    scan: usize,
    /// Reserved receive markers.
    markers: Vec<Marker>,
    /// Distinguishes consecutive reserved markers.
    next_marker: i64,
}

/// A tiny in-memory mailbox implementing the engine's [`Mailbox`] seam.
///
/// A **copy of the semantics** of `tests/receive.rs`'s `TestMailbox` (that suite is
/// not editable, and duplicating ~90 lines of a documented seam is cheaper than
/// coupling the census to another suite's private harness). It reproduces the C
/// message-queue contract the receive family drives
/// (`../../erts/emulator/beam/erl_proc_sig_queue.h:2225-2394`): peek at the save
/// position, advance it, remove the peeked message and reset, reset.
///
/// Three differences from the `receive.rs` original: [`push`](ProbeMailbox::push)
/// also sets the scan position when the queue was empty (a pre-seeded mailbox
/// behaves exactly as if the messages had been delivered to a suspended receiver),
/// [`messages`](ProbeMailbox::messages) and [`scan`](ProbeMailbox::scan) are public
/// for post-run assertions, and the type is `Clone` so the row keeps a handle while
/// the context owns the `Box<dyn Mailbox>`.
#[derive(Clone)]
pub struct ProbeMailbox {
    state: Rc<RefCell<MailboxState>>,
    /// The term `receive_marker_reserve` returns when the queue is empty (the C
    /// `am_undefined`).
    undefined: Eterm,
}

impl ProbeMailbox {
    /// An empty mailbox whose empty-queue marker request yields `undefined`.
    pub fn new(undefined: Eterm) -> ProbeMailbox {
        ProbeMailbox {
            state: Rc::new(RefCell::new(MailboxState::default())),
            undefined,
        }
    }

    /// A mailbox pre-seeded with `messages`, in arrival order.
    pub fn with_messages(undefined: Eterm, messages: Vec<Eterm>) -> ProbeMailbox {
        let mailbox = ProbeMailbox::new(undefined);
        for message in messages {
            mailbox.push(message);
        }
        mailbox
    }

    /// Queue `message` as if it had been delivered to this mailbox.
    pub fn push(&self, message: Eterm) {
        let mut state = self.state.borrow_mut();
        if state.messages.is_empty() {
            // A fresh delivery to an empty queue is also where the scan starts; a
            // saturating index left past the end by a drained scan must not hide it.
            state.scan = 0;
        }
        state.messages.push(message);
    }

    /// The queued messages, in arrival order.
    pub fn messages(&self) -> Vec<Eterm> {
        self.state.borrow().messages.clone()
    }

    /// The current scan position.
    pub fn scan(&self) -> usize {
        self.state.borrow().scan
    }
}

impl Mailbox for ProbeMailbox {
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<(), BeamError> {
        // A probe's `send` always targets this very mailbox: a real host resolves
        // `to` to a process, and the census row documents that its `to` argument is
        // only a placeholder.
        let _ = to;
        self.push(message);
        Ok(())
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>, BeamError> {
        let state = self.state.borrow();
        Ok(state.messages.get(state.scan).copied())
    }

    fn receive_skip(&mut self) -> Result<(), BeamError> {
        self.state.borrow_mut().scan += 1;
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<(), BeamError> {
        let mut state = self.state.borrow_mut();
        let scan = state.scan;
        if scan < state.messages.len() {
            state.messages.remove(scan);
        }
        state.scan = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<(), BeamError> {
        self.state.borrow_mut().scan = 0;
        Ok(())
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm, BeamError> {
        let undefined = self.undefined;
        let mut state = self.state.borrow_mut();
        if state.messages.is_empty() {
            // C: an empty queue has no position to record.
            return Ok(undefined);
        }
        let n = state.next_marker;
        state.next_marker += 1;
        let term = beam_term::make_small(9000 + n);
        let position = state.scan;
        state.markers.push(Marker {
            term,
            reference: None,
            position,
        });
        Ok(term)
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<(), BeamError> {
        // The C only binds a live (small/big) marker id; `undefined` and any other
        // term is a silent no-op (`erl_proc_sig_queue.h:2238-2263`).
        if !is_small(marker) {
            return Ok(());
        }
        let mut state = self.state.borrow_mut();
        if let Some(entry) = state.markers.iter_mut().find(|m| m.term == marker) {
            entry.reference = Some(reference);
        }
        Ok(())
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<(), BeamError> {
        let mut state = self.state.borrow_mut();
        let position = state
            .markers
            .iter()
            .find(|m| m.reference == Some(reference))
            .map(|m| m.position);
        if let Some(position) = position {
            state.scan = position;
        }
        Ok(())
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<(), BeamError> {
        let mut state = self.state.borrow_mut();
        if let Some(index) = state
            .markers
            .iter()
            .position(|m| m.reference == Some(reference))
        {
            state.markers.remove(index);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The declared expectation + the observed outcome
// ---------------------------------------------------------------------------

/// What a row **declares** the engine will do with its instruction.
///
/// A declared expectation is what turns "the probe produced some outcome" into a
/// real claim about the engine: the shape is the instruction's own defined BEAM
/// behaviour (the fall-through, the `fail` branch, the suspend or the exception
/// C/the compiler define for it), never an engine diagnostic.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Expect {
    /// The instruction falls through to the success sink; `Some(value)` pins the
    /// returned term (compare with `==`, so an immediate — a small int or an atom —
    /// is exact), `None` pins only the shape.
    Returned(Option<Eterm>),
    /// The instruction takes its declared `fail`/branch sink — the run returns the
    /// distinct [`ProbeAtoms::fail_mark`] atom, which no success path can produce.
    Branched,
    /// The instruction suspends (a `receive` with no message); `Some(value)` pins
    /// the value the **resume** produced.
    Suspends(Option<Eterm>),
    /// The instruction raises its **own** defined BEAM exception: `class` and the
    /// reason **atom** name (e.g. `error`/`function_clause`). A structured reason is
    /// observed by landing it in a catch frame instead (see [`Expect::Branched`]).
    Raises {
        /// The exception class the instruction raises.
        class: ExceptionClass,
        /// The reason atom's name.
        reason: &'static str,
    },
    /// No outcome declared yet: a round-2 row driven for observability. Any outcome
    /// matches **except** an engine capability diagnostic, which is always reported
    /// as a gap regardless of the declaration.
    Unspecified,
}

impl Expect {
    /// A one-line rendering for the census print.
    pub fn describe(&self) -> String {
        match self {
            Expect::Returned(None) => "returns".to_string(),
            Expect::Returned(Some(value)) => format!("returns {}", describe_raw(*value)),
            Expect::Branched => "branches to fail".to_string(),
            Expect::Suspends(None) => "suspends".to_string(),
            Expect::Suspends(Some(value)) => {
                format!("suspends then returns {}", describe_raw(*value))
            }
            Expect::Raises { class, reason } => format!("raises {}:{}", class.as_str(), reason),
            Expect::Unspecified => "unspecified".to_string(),
        }
    }
}

/// What a run **observed**.
#[derive(Clone, PartialEq, Debug)]
pub enum Observed {
    /// The run reached the success sink with this `x(0)`.
    Returned(Eterm),
    /// The run reached the fail sink (its distinct mark atom).
    Branched,
    /// The run suspended at `(func, pc)` and the resume suspended again.
    Suspended {
        /// The resume address the engine reported.
        func: u32,
        /// The resume address the engine reported.
        pc: u32,
    },
    /// The run suspended and the resume returned this value.
    Resumed {
        /// The resume address the engine reported.
        func: u32,
        /// The resume address the engine reported.
        pc: u32,
        /// The resumed value.
        value: Eterm,
    },
    /// The run ended by raising a BEAM exception (class + reason atom name).
    Raised {
        /// The exception class.
        class: ExceptionClass,
        /// The reason atom's name (`<structured>` for a non-atom reason).
        reason: String,
    },
    /// The run ended with a typed error that is **not** an exception.
    Failed,
}

impl Observed {
    /// The term the run produced, when it produced one (`x(0)` on either sink, or the
    /// resumed value).
    ///
    /// The term is only valid while the [`ProbeRun`] it came from is alive.
    pub fn value(&self) -> Option<Eterm> {
        match self {
            Observed::Returned(term) => Some(*term),
            Observed::Resumed { value, .. } => Some(*value),
            Observed::Branched | Observed::Suspended { .. } | Observed::Raised { .. } => None,
            Observed::Failed => None,
        }
    }
}

/// The verdict of comparing a run to its declared expectation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// The engine did exactly what the row declared.
    Matched,
    /// The run reported an **engine capability diagnostic** (`unsupported_opcode: …`)
    /// — a real engine gap, never a probe artifact.
    Diagnostic,
    /// The outcome contradicted the row's declared expectation.
    Contradicted,
}

// ---------------------------------------------------------------------------
// The probe row
// ---------------------------------------------------------------------------

/// One census row: an instruction, everything it needs to reach its defined
/// behaviour, and the expectation that behaviour is checked against.
///
/// Every field defaults to "nothing", so [`Probe::new`] plus the builder methods is
/// the whole API. `Probe` is consumed by [`Probe::run`] (the hooks are boxed
/// closures, so they cannot be lent out twice).
pub struct Probe {
    /// The instruction under test.
    pub instr: Instr,
    /// Instructions emitted after `ENTRY_LABEL`, before `instr` (a `try` frame, a
    /// `bs_start_match3`, the register seeding a later operand reads, …).
    pub setup: Vec<Instr>,
    /// Instructions emitted after `instr`, before the success sink (a non-tail
    /// call's `deallocate`, a move of the result into `x(0)`, …).
    pub epilogue: Vec<Instr>,
    /// Extra `ImpT` entries. Import 0 is always `erlang:self/0`; these are appended
    /// after it (so `ImportRef(1)` is the first extra).
    pub imports: Vec<Import>,
    /// The module's `FunT` lambda descriptors (`make_fun3`'s index addresses this).
    pub funs: Vec<FunDescriptor>,
    /// The module's `Recs` native-record definitions.
    pub records: Vec<RecordDefinition>,
    /// The module's `StrT` string-table bytes.
    pub strings: Vec<u8>,
    /// The **declared** function arity (what `func_info` names, the `argv`
    /// derivation's bound). Defaults to the argument count.
    pub function_arity: Option<u32>,
    /// Builds the run's arguments; `x(0..args.len())` is seeded from them. The
    /// builder runs against the run's own context, so the terms live in the heap the
    /// machine allocates from.
    pub args: Option<fn(&mut MinimalContext) -> Vec<Eterm>>,
    /// Atoms to intern into the probe module's table **beyond** the canonical
    /// universe (a name only this row needs).
    pub extra_atoms: Vec<&'static str>,
    /// The mailbox to install (pre-seeded through [`ProbeMailbox::push`]).
    pub mailbox: Option<ProbeMailbox>,
    /// A message appended to the mailbox before a suspension is resumed (what a real
    /// wakeup looks like).
    pub resume_message: Option<Eterm>,
    /// The BIF hook to install.
    pub bif: Option<BifHook>,
    /// The fun-application hook to install.
    pub apply: Option<ApplyHook>,
    /// The cross-module-call resolver to install.
    pub external: Option<ExternalResolver>,
    /// The local-closure resolver to install.
    pub local_fun: Option<LocalFunResolver>,
    /// The local-call (call-as-BIF) resolver to install.
    pub local_call: Option<LocalCallResolver>,
    /// Code blocks appended **after** the fail sink (a callee leaf, a lambda body, a
    /// catch handler), each introduced by its own `Instr::Label(aux_label(n))`.
    pub extra_blocks: Vec<Instr>,
    /// The declared expectation.
    pub expect: Expect,
    /// A short human-readable note printed beside the row (why the row is shaped the
    /// way it is).
    pub note: &'static str,
}

impl Probe {
    /// A probe for `instr`, declaring `expect`, with no prologue and no hooks.
    pub fn new(instr: Instr, expect: Expect) -> Probe {
        Probe {
            instr,
            setup: Vec::new(),
            epilogue: Vec::new(),
            imports: Vec::new(),
            funs: Vec::new(),
            records: Vec::new(),
            strings: Vec::new(),
            function_arity: None,
            args: None,
            extra_atoms: Vec::new(),
            mailbox: None,
            resume_message: None,
            bif: None,
            apply: None,
            external: None,
            local_fun: None,
            local_call: None,
            extra_blocks: Vec::new(),
            expect,
            note: "",
        }
    }

    /// Set the prologue.
    pub fn setup(mut self, setup: Vec<Instr>) -> Probe {
        self.setup = setup;
        self
    }

    /// Set the epilogue.
    pub fn epilogue(mut self, epilogue: Vec<Instr>) -> Probe {
        self.epilogue = epilogue;
        self
    }

    /// Append one extra import (after `erlang:self/0`).
    pub fn import(mut self, module: Atom, function: Atom, arity: u32) -> Probe {
        self.imports.push(Import {
            module,
            function,
            arity,
        });
        self
    }

    /// Set the `FunT` descriptors.
    pub fn funs(mut self, funs: Vec<FunDescriptor>) -> Probe {
        self.funs = funs;
        self
    }

    /// Set the `Recs` definitions.
    pub fn records(mut self, records: Vec<RecordDefinition>) -> Probe {
        self.records = records;
        self
    }

    /// Set the `StrT` bytes.
    pub fn strings(mut self, strings: Vec<u8>) -> Probe {
        self.strings = strings;
        self
    }

    /// Declare the function arity (defaults to the argument count).
    pub fn function_arity(mut self, arity: u32) -> Probe {
        self.function_arity = Some(arity);
        self
    }

    /// Set the argument builder.
    pub fn args(mut self, args: fn(&mut MinimalContext) -> Vec<Eterm>) -> Probe {
        self.args = Some(args);
        self
    }

    /// Intern one extra atom into the probe module's table.
    pub fn extra_atom(mut self, name: &'static str) -> Probe {
        self.extra_atoms.push(name);
        self
    }

    /// Install (and keep a handle to) a mailbox.
    pub fn mailbox(mut self, mailbox: ProbeMailbox) -> Probe {
        self.mailbox = Some(mailbox);
        self
    }

    /// Append `message` to the mailbox before a suspension is resumed.
    pub fn resume_message(mut self, message: Eterm) -> Probe {
        self.resume_message = Some(message);
        self
    }

    /// Install the BIF hook.
    pub fn bif(mut self, hook: BifHook) -> Probe {
        self.bif = Some(hook);
        self
    }

    /// Install the fun-application hook.
    pub fn apply_hook(mut self, hook: ApplyHook) -> Probe {
        self.apply = Some(hook);
        self
    }

    /// Install the cross-module-call resolver.
    pub fn external(mut self, resolver: ExternalResolver) -> Probe {
        self.external = Some(resolver);
        self
    }

    /// Install the local-closure resolver.
    pub fn local_fun(mut self, resolver: LocalFunResolver) -> Probe {
        self.local_fun = Some(resolver);
        self
    }

    /// Install the local-call resolver.
    pub fn local_call(mut self, resolver: LocalCallResolver) -> Probe {
        self.local_call = Some(resolver);
        self
    }

    /// Append an aux code block (its own `Instr::Label(aux_label(n))` included).
    pub fn block(mut self, block: Vec<Instr>) -> Probe {
        self.extra_blocks.extend(block);
        self
    }

    /// Set the row's explanatory note.
    pub fn note(mut self, note: &'static str) -> Probe {
        self.note = note;
        self
    }

    /// Build the probe module described by this row.
    ///
    /// One function (`probe:f/<arity>`) whose code is
    /// `ENTRY ++ setup ++ [instr] ++ epilogue ++ SUCCESS ++ [FAIL_SINK] ++ extra`,
    /// with `Function::labels` derived from that final code.
    pub fn build_module(&self, atoms: &ProbeAtoms) -> Module {
        let mut module = Module::default();
        module.name = atoms.module();
        module.atom_table = atoms.table();
        for name in &self.extra_atoms {
            module.atom_table.intern(name);
        }
        // The canonical import every call/BIF probe may address as `ImportRef(0)`.
        module.imports.push(Import {
            module: atoms.named("erlang"),
            function: atoms.named("self"),
            arity: 0,
        });
        module.imports.extend(self.imports.iter().cloned());
        module.funs = self.funs.clone();
        module.records = self.records.clone();
        module.strings = self.strings.clone();

        let mut code: Vec<Instr> = Vec::new();
        code.push(Instr::Label(ENTRY_LABEL));
        code.extend(self.setup.iter().cloned());
        code.push(self.instr.clone());
        code.extend(self.epilogue.iter().cloned());
        // The success sink: a bare `return` on the bottom frame (the `Halt`
        // continuation) finishes the run with `x(0)`.
        code.push(Instr::Label(SUCCESS_LABEL));
        code.push(Instr::Return);
        // The fail sink: a DISTINCT mark in `x(0)`, so a declared `fail` branch can
        // never be mistaken for a success fall-through.
        code.push(Instr::Label(FAIL_SINK_LABEL));
        code.push(Instr::Move {
            src: Operand::Atom(atoms.named("probe_fail")),
            dst: Operand::X(0),
        });
        code.push(Instr::Return);
        code.extend(self.extra_blocks.iter().cloned());

        let labels = labels_of(&code);
        let arity = self.function_arity.unwrap_or_else(|| {
            self.args
                .map(|build| build_len_hint(build))
                .unwrap_or_default()
        });
        module.functions.push(Function {
            func_label: ENTRY_LABEL,
            entry_label: ENTRY_LABEL,
            module: atoms.module(),
            name: atoms.function(),
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        module
    }

    /// Drive this probe through the real public execution entry and report what
    /// happened.
    ///
    /// The run uses [`Machine::run_label_outcome`] (so a legitimate suspend is
    /// observable, not folded into a typed error) and, when it suspends, resumes at
    /// the reported `(func, pc)` — appending [`Probe::resume_message`] first when the
    /// row declares one, exactly as a delivered message would.
    pub fn run(mut self, atoms: &ProbeAtoms) -> ProbeRun {
        let module = self.build_module(atoms);
        let mut ctx = MinimalContext::new();
        // The argument builder runs against the run's own context, so the terms it
        // builds live in the very heap the machine allocates from.
        let args = self
            .args
            .map(|build| build(&mut ctx))
            .unwrap_or_default();
        let arity = args.len() as u32;
        // The hooks are boxed closures, so they are *moved* into the context (the
        // probe is consumed by `run`).
        if let Some(hook) = self.bif.take() {
            ctx.set_bif_hook(hook);
        }
        if let Some(hook) = self.apply.take() {
            ctx.set_apply_hook(hook);
        }
        if let Some(resolver) = self.external.take() {
            ctx.set_external_resolver(resolver);
        }
        if let Some(resolver) = self.local_fun.take() {
            ctx.set_local_fun_resolver(resolver);
        }
        if let Some(resolver) = self.local_call.take() {
            ctx.set_local_call_resolver(resolver);
        }
        if let Some(mailbox) = &self.mailbox {
            ctx.set_mailbox(Box::new(mailbox.clone()));
        }
        let expect = self.expect;
        let note = self.note;

        let mut machine = Machine::new();
        let result =
            machine.run_label_outcome(&module, ENTRY_LABEL, arity, &args, &mut ctx);
        let (observed, detail) = match result {
            Ok(RunOutcome::Returned(term)) => observed_returned(atoms, &module, term),
            Ok(RunOutcome::Wait { func, pc }) => {
                if let (Some(mailbox), Some(message)) = (&self.mailbox, self.resume_message) {
                    mailbox.push(message);
                }
                let suspended = format!("suspended at ({func},{pc})");
                match machine.resume_outcome(&module, func, pc, &mut ctx) {
                    Ok(RunOutcome::Returned(term)) => (
                        Observed::Resumed {
                            func,
                            pc,
                            value: term,
                        },
                        format!("{suspended}, resumed to {}", render_term(&module, term)),
                    ),
                    Ok(RunOutcome::Wait { func, pc }) => (
                        Observed::Suspended { func, pc },
                        format!("{suspended}, suspended again at ({func},{pc})"),
                    ),
                    Err(error) => {
                        let (observed, detail) = observed_error(&module, error);
                        (observed, format!("{suspended}, then {detail}"))
                    }
                }
            }
            Err(error) => observed_error(&module, error),
        };
        ProbeRun {
            expect,
            note,
            module,
            machine,
            ctx,
            mailbox: self.mailbox.clone(),
            observed,
            detail,
        }
    }
}

/// The argument count a [`Probe::args`] builder produces, for the default function
/// arity.
///
/// A builder cannot be called for free (it may allocate), so the default function
/// arity is taken from a **temporary** context: the count is all that is needed, and
/// calling the builder against a scratch context is harmless (its terms are
/// dropped).
fn build_len_hint(build: fn(&mut MinimalContext) -> Vec<Eterm>) -> u32 {
    let mut scratch = MinimalContext::new();
    build(&mut scratch).len() as u32
}

/// Classify a returned term: the fail mark is [`Observed::Branched`], anything else
/// is [`Observed::Returned`].
fn observed_returned(atoms: &ProbeAtoms, module: &Module, term: Eterm) -> (Observed, String) {
    if term == atoms.fail_mark() {
        (
            Observed::Branched,
            "branched to the fail sink (probe_fail)".to_string(),
        )
    } else {
        (
            Observed::Returned(term),
            format!("returned {}", render_term(module, term)),
        )
    }
}

/// Classify a typed error: a `Raise` is [`Observed::Raised`], anything else is
/// [`Observed::Failed`]. The detail string always carries the full `Debug` text (so
/// the [`UNCLAIMED_PREFIX`] sentinel is visible) plus the reason atom name when the
/// error has one.
fn observed_error(_module: &Module, error: BeamError) -> (Observed, String) {
    let mut text = format!("{error:?}");
    if let Some(atom) = error.as_atom() {
        text.push_str(" => ");
        text.push_str(atom);
    }
    match &error {
        BeamError::Raise(raise) => {
            let reason = raise
                .reason
                .as_atom()
                .map(str::to_string)
                .unwrap_or_else(|| "<structured>".to_string());
            let observed = Observed::Raised {
                class: raise.class,
                reason,
            };
            (observed, format!("raised {text}"))
        }
        _ => (Observed::Failed, format!("failed {text}")),
    }
}

/// Render a term for the census print: a small integer as its value, an atom by name
/// (through the probe module's table), `[]`, else the raw term.
pub fn render_term(module: &Module, term: Eterm) -> String {
    describe_in(module, term)
}

/// [`render_term`] without a module (a bare term: small ints still render).
fn describe_raw(term: Eterm) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if beam_term::is_nil(term) {
        return "[]".to_string();
    }
    format!("{term:?}")
}

/// The module-aware renderer behind [`render_term`].
fn describe_in(module: &Module, term: Eterm) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if beam_term::is_atom(term) {
        return match module.atom_name(beam_term::atom_val(term)) {
            Some(name) => name.to_string(),
            None => format!("<atom#{}>", beam_term::atom_val(term).raw()),
        };
    }
    if beam_term::is_nil(term) {
        return "[]".to_string();
    }
    if beam_engine::terms::is_fun(term) {
        return "<fun>".to_string();
    }
    format!("{term:?}")
}

/// Map every `Instr::Label` in `code` to its index (the loader's contract: one
/// module-wide label namespace).
pub fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// One probe's complete result.
///
/// It owns `module`, `machine` and `ctx` because every [`Eterm`] in
/// [`ProbeRun::observed`] points into the context arena or the module's literal
/// area; assertions may read them only while this value is alive.
pub struct ProbeRun {
    /// The declared expectation of the row that produced this run.
    pub expect: Expect,
    /// The row's explanatory note.
    pub note: &'static str,
    /// The probe module the run executed.
    pub module: Module,
    /// The machine after the run (intact for `pending_exception`, register reads, …).
    pub machine: Machine,
    /// The context that owns the run's heap (kept alive for the terms above).
    pub ctx: MinimalContext,
    /// The mailbox the run drove, when the row installed one (shared state, so a
    /// post-run assertion sees every send).
    pub mailbox: Option<ProbeMailbox>,
    /// What the run did.
    pub observed: Observed,
    /// The full human-readable detail (including the typed error text when the run
    /// ended in one, so [`UNCLAIMED_PREFIX`] is always visible).
    pub detail: String,
}

impl ProbeRun {
    /// Whether the run named an engine capability diagnostic ([`ENGINE_DIAGNOSTIC_PREFIX`]).
    pub fn is_diagnostic(&self) -> bool {
        self.detail.contains(ENGINE_DIAGNOSTIC_PREFIX)
    }

    /// Whether the run hit the family-dispatch "no arm, no family" sentinel.
    pub fn is_unclaimed(&self) -> bool {
        self.detail.contains(UNCLAIMED_PREFIX)
    }

    /// Compare the observed outcome to `expect`.
    ///
    /// A run that reports an engine capability diagnostic is **always**
    /// [`Verdict::Diagnostic`], whatever the row declared: an engine limitation must
    /// never be able to satisfy a declared expectation.
    pub fn verdict(&self, expect: &Expect) -> Verdict {
        if self.is_diagnostic() {
            return Verdict::Diagnostic;
        }
        let matched = match (expect, &self.observed) {
            (Expect::Unspecified, _) => true,
            (Expect::Returned(pinned), Observed::Returned(term)) => match pinned {
                Some(value) => term == value,
                None => true,
            },
            (Expect::Branched, Observed::Branched) => true,
            (Expect::Suspends(pinned), Observed::Resumed { value, .. }) => match pinned {
                Some(value_pinned) => value == value_pinned,
                None => true,
            },
            (Expect::Suspends(None), Observed::Suspended { .. }) => true,
            (
                Expect::Raises { class, reason },
                Observed::Raised {
                    class: got_class,
                    reason: got_reason,
                },
            ) => got_class == class && got_reason == *reason,
            _ => false,
        };
        if matched {
            Verdict::Matched
        } else {
            Verdict::Contradicted
        }
    }

    /// The row's one-line census detail: the expectation, the observation, the
    /// verdict and the note.
    pub fn report(&self) -> String {
        let verdict = self.verdict(&self.expect);
        let verdict_text = match verdict {
            Verdict::Matched => "ok",
            Verdict::Diagnostic => "ENGINE-DIAGNOSTIC",
            Verdict::Contradicted => "CONTRADICTS-EXPECTATION",
        };
        if self.note.is_empty() {
            format!("{verdict_text}: {}", self.detail)
        } else {
            format!("{verdict_text}: {} — {}", self.detail, self.note)
        }
    }
}
