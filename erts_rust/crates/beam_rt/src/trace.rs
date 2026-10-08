//! Process tracing (contract A.8): the `erlang:trace/…`, `erlang:trace_info/…` and
//! `erlang:trace_delivered/1` BIF machinery — the per-process **trace-flag state**.
//!
//! Ordinary tracing lets one process (the *tracer*) observe another's events. A runtime's half
//! of it is small and entirely local state:
//!
//! * **the trace flags** — the `[send, receive, call, return_to, …, silent]` bit set a tracer
//!   selected ([`TraceFlags`]), plus the flags with no canonical atom (`return_from`,
//!   `cpu_timestamp`, which this stage carries but never reports);
//! * **the tracer** — the pid the tracees report to ([`TraceEntry::tracer`]);
//! * **the per-pid entries** — one [`TraceEntry`] per traced process ([`TraceState`]).
//!
//! The whole model is **pure**: [`TraceState`] holds no runtime borrow and allocates nothing,
//! so it can be unit-tested on its own. Only the term-building half
//! ([`RuntimeContext::trace_info_2`], [`RuntimeContext::trace_delivered_1`] and the
//! `erts_internal:trace/3` core) lives in the BIF layer, because a term needs the engine's heap
//! seam.
//!
//! # What is implemented
//!
//! * `erlang:trace_info(Pid, Item)` for the four items `flags`, `tracer`, `match_spec` and
//!   `traced`, plus the OTP-30 `all` item (the list of the four `{Item, Value}` pairs);
//! * `erlang:trace_delivered(Tracee)` — a fresh reference is sent back to the caller as
//!   `{trace_delivered, Tracee, Ref}` and returned;
//! * `erts_internal:trace/3` (the body `erlang:trace/2,3` calls) for the process targets
//!   `Pid | all | existing | existing_processes | processes`;
//! * `erts_internal:trace_pattern/3` (the body `erlang:trace_pattern/2,3` calls) — the
//!   **function** trace patterns. [`TraceState`] owns a [`PatternTable`]: one entry per
//!   installed `{M,F,A}` pattern (wildcard-aware — [`MfaPattern::matches`]) holding its
//!   [`PatternFlags`] and its [`MatchSpec`]. The BIF's answer is the number of **exported**
//!   functions of the modules this runtime has loaded that the pattern matches
//!   ([`MfaPattern::count_matches`]).
//!
//! # Deliberately out of scope (a later stage)
//!
//! * **Emitting `{trace, …}`/`{trace_ts, …}` event messages.** Nothing here is wired into the
//!   engine's send/receive/call paths, so setting a flag — or installing a trace *pattern* —
//!   never produces an event. The flag/pattern state is real (and `trace_info`, plus the
//!   pattern table, report it), but no message is emitted. [`PatternTable::matches`] is the
//!   predicate a future `beam_engine` per-call hook calls to decide whether a
//!   `call`/`return_from`/`return_to` event should be emitted; until that hook exists it is
//!   only a query.
//! * **Match-spec compilation.** A `[match_spec()]` argument is shape-checked and stored
//!   verbatim ([`MatchSpec::Functions`]); it is never compiled and never applied, so it
//!   cannot filter an event (there are none to filter). `trace_info`'s `match_spec` item is a
//!   **process** match spec, which `trace_pattern` (a *function*-level facility) cannot set,
//!   so it stays `[]`.
//! * The `new`/`new_processes`/`ports`/`existing_ports`/`new_ports` PidSpecs of
//!   `erts_internal:trace/3` — they are the honest typed capability gap (port/`new`-style
//!   trace targets are not modelled).
//! * Tracing **functions** through `trace_info({M,F,A}, …)`: a non-pid first argument is
//!   `badarg`, as the C is for an MFA this stage cannot resolve.
//!
//! # Ownership of the stored match specs
//!
//! A [`MatchSpec::Functions`] retains the raw [`Eterm`] words of the `[match_spec()]`
//! argument. That is the same M1 contract the persistent-term store documents: the runtime
//! never resets the heap a BIF argument lives on and there is no collector, so the words stay
//! valid for the runtime's life — and when Stage 4's collector lands, this table is a root it
//! must trace exactly like [`crate::context`]'s persistent-term store.
//!
//! Legacy C ground truth (read-only): `../../erts/emulator/beam/erl_bif_trace.c` (`trace_3`,
//! `trace_info_2`, `trace_delivered_1`, `trace_pattern_3`) and
//! `../../erts/emulator/beam/erl_trace.c`.

use std::collections::HashMap;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_process::ProcessId;
use beam_term::{Eterm, NIL};

use crate::context::RuntimeContext;

// The internal flag bits. The bits themselves are **not** observable: only the atom set and the
// decode order are. The three atoms with no canonical reporting atom (`all`, `return_from`,
// `cpu_timestamp`) still need a bit so `all` expands to a stable set and an accepted flag is
// never dropped.
const SEND: u32 = 1 << 0;
const RECEIVE: u32 = 1 << 1;
const CALL: u32 = 1 << 2;
const RETURN_TO: u32 = 1 << 3;
const RETURN_FROM: u32 = 1 << 4;
const PROCS: u32 = 1 << 5;
const PORTS: u32 = 1 << 6;
const RUNNING: u32 = 1 << 7;
const EXITING: u32 = 1 << 8;
const RUNNING_PROCS: u32 = 1 << 9;
const RUNNING_PORTS: u32 = 1 << 10;
const GARBAGE_COLLECTION: u32 = 1 << 11;
const TIMESTAMP: u32 = 1 << 12;
const MONOTONIC_TIMESTAMP: u32 = 1 << 13;
const STRICT_MONOTONIC_TIMESTAMP: u32 = 1 << 14;
const CPU_TIMESTAMP: u32 = 1 << 15;
const ARITY: u32 = 1 << 16;
const SET_ON_SPAWN: u32 = 1 << 17;
const SET_ON_FIRST_SPAWN: u32 = 1 << 18;
const SET_ON_LINK: u32 = 1 << 19;
const SET_ON_FIRST_LINK: u32 = 1 << 20;
const SILENT: u32 = 1 << 21;
const SCHEDULER_ID: u32 = 1 << 22;

/// Every base trace flag at once — what the `all` atom expands to.
const ALL_BITS: u32 = SEND
    | RECEIVE
    | CALL
    | RETURN_TO
    | RETURN_FROM
    | PROCS
    | PORTS
    | RUNNING
    | EXITING
    | RUNNING_PROCS
    | RUNNING_PORTS
    | GARBAGE_COLLECTION
    | TIMESTAMP
    | MONOTONIC_TIMESTAMP
    | STRICT_MONOTONIC_TIMESTAMP
    | CPU_TIMESTAMP
    | ARITY
    | SET_ON_SPAWN
    | SET_ON_FIRST_SPAWN
    | SET_ON_LINK
    | SET_ON_FIRST_LINK
    | SILENT
    | SCHEDULER_ID;

/// The canonical decode order of the flag atoms, exactly as the oracle's `trace_info(P, flags)`
/// reports them (observed on OTP 29.1.1 / erts-17.1). The order is **not** sorted, so it is
/// pinned rather than derived; a flag with no canonical atom (`return_from`, `cpu_timestamp`,
/// `all`) is deliberately absent and is therefore omitted from the decode.
const CANONICAL: &[(&str, u32)] = &[
    ("running_procs", RUNNING_PROCS),
    ("running_ports", RUNNING_PORTS),
    ("ports", PORTS),
    ("scheduler_id", SCHEDULER_ID),
    ("silent", SILENT),
    ("return_to", RETURN_TO),
    ("arity", ARITY),
    ("garbage_collection", GARBAGE_COLLECTION),
    ("exiting", EXITING),
    ("running", RUNNING),
    ("set_on_first_link", SET_ON_FIRST_LINK),
    ("set_on_link", SET_ON_LINK),
    ("set_on_first_spawn", SET_ON_FIRST_SPAWN),
    ("procs", PROCS),
    ("call", CALL),
    ("set_on_spawn", SET_ON_SPAWN),
    ("receive", RECEIVE),
    ("send", SEND),
    ("monotonic_timestamp", MONOTONIC_TIMESTAMP),
    ("strict_monotonic_timestamp", STRICT_MONOTONIC_TIMESTAMP),
    ("timestamp", TIMESTAMP),
];

/// A process's trace-flag bit set.
///
/// The bits are internal — only the atom set and its decode order are observable — but the
/// value round-trips through [`flags_from_atoms`]/[`flags_to_atoms`] and is what a
/// [`TraceEntry`] stores.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct TraceFlags(u32);

/// Decodes flag atoms into a [`TraceFlags`].
///
/// The `all` atom expands to **every** base flag ([`ALL_BITS`]); repeated atoms are deduplicated
/// by the bitwise OR. An **unknown** atom makes the whole decode `None` (the caller maps that to
/// the oracle's `badarg`).
pub fn flags_from_atoms(atoms: &[&str]) -> Option<TraceFlags> {
    let mut bits = 0u32;
    for name in atoms {
        bits |= match *name {
            "all" => ALL_BITS,
            "send" => SEND,
            "receive" => RECEIVE,
            "call" => CALL,
            "return_to" => RETURN_TO,
            "return_from" => RETURN_FROM,
            "procs" => PROCS,
            "ports" => PORTS,
            "running" => RUNNING,
            "exiting" => EXITING,
            "running_procs" => RUNNING_PROCS,
            "running_ports" => RUNNING_PORTS,
            "garbage_collection" => GARBAGE_COLLECTION,
            "timestamp" => TIMESTAMP,
            "monotonic_timestamp" => MONOTONIC_TIMESTAMP,
            "strict_monotonic_timestamp" => STRICT_MONOTONIC_TIMESTAMP,
            "cpu_timestamp" => CPU_TIMESTAMP,
            "arity" => ARITY,
            "set_on_spawn" => SET_ON_SPAWN,
            "set_on_first_spawn" => SET_ON_FIRST_SPAWN,
            "set_on_link" => SET_ON_LINK,
            "set_on_first_link" => SET_ON_FIRST_LINK,
            "silent" => SILENT,
            "scheduler_id" => SCHEDULER_ID,
            _ => return None,
        };
    }
    Some(TraceFlags(bits))
}

/// Encodes a [`TraceFlags`] as the flag atoms `erlang:trace_info(Pid, flags)` reports.
///
/// The atoms come back in [`CANONICAL`] order; a set bit with no canonical atom
/// (`return_from`, `cpu_timestamp`) is omitted, and `all` is never emitted as the atom `all`
/// (it has already been expanded into the individual flags by [`flags_from_atoms`]).
pub fn flags_to_atoms(flags: TraceFlags) -> Vec<&'static str> {
    CANONICAL
        .iter()
        .filter(|(_, bit)| flags.0 & bit != 0)
        .map(|(name, _)| *name)
        .collect()
}

// The `trace_pattern` flag bits — the seven `TFLAG_*` atoms a `trace_pattern` `FlagList`
// accepts. They are a **different** atom set from the process flags above: `call` (a process
// flag) is *not* a `trace_pattern` flag, and the oracle raises `badarg` for it.
const PATTERN_LOCAL: u32 = 1 << 0;
const PATTERN_GLOBAL: u32 = 1 << 1;
const PATTERN_META: u32 = 1 << 2;
const PATTERN_CALL_COUNT: u32 = 1 << 3;
const PATTERN_CALL_TIME: u32 = 1 << 4;
const PATTERN_CALL_MEMORY: u32 = 1 << 5;
const PATTERN_SILENT: u32 = 1 << 6;

/// The `trace_pattern` decode/report order — the seven flag atoms, pinned rather than
/// derived (the order is this runtime's own; no oracle surface reports it back).
const PATTERN_CANONICAL: &[(&str, u32)] = &[
    ("local", PATTERN_LOCAL),
    ("global", PATTERN_GLOBAL),
    ("meta", PATTERN_META),
    ("call_count", PATTERN_CALL_COUNT),
    ("call_time", PATTERN_CALL_TIME),
    ("call_memory", PATTERN_CALL_MEMORY),
    ("silent", PATTERN_SILENT),
];

/// A `trace_pattern` flag bit set — which of the seven valid `FlagList` atoms were given.
///
/// The bits are internal; the atom set is what round-trips through
/// [`pattern_flags_from_atoms`]/[`pattern_flags_to_atoms`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct PatternFlags(u32);

/// Decodes a `trace_pattern` `FlagList` into a [`PatternFlags`].
///
/// Only the seven per-function tracing flags are valid here. An **unknown** atom makes the
/// whole decode `None`, which the caller maps to the oracle's `badarg` — `[call]` is the
/// oracle-observed rejection (a *process* flag, not a `trace_pattern` flag). An empty list is
/// zero flags, not an error.
pub fn pattern_flags_from_atoms(atoms: &[&str]) -> Option<PatternFlags> {
    let mut bits = 0u32;
    for name in atoms {
        bits |= match *name {
            "local" => PATTERN_LOCAL,
            "global" => PATTERN_GLOBAL,
            "meta" => PATTERN_META,
            "call_count" => PATTERN_CALL_COUNT,
            "call_time" => PATTERN_CALL_TIME,
            "call_memory" => PATTERN_CALL_MEMORY,
            "silent" => PATTERN_SILENT,
            _ => return None,
        };
    }
    Some(PatternFlags(bits))
}

/// Encodes a [`PatternFlags`] as its atoms, in [`PATTERN_CANONICAL`] order.
pub fn pattern_flags_to_atoms(flags: PatternFlags) -> Vec<&'static str> {
    PATTERN_CANONICAL
        .iter()
        .filter(|(_, bit)| flags.0 & bit != 0)
        .map(|(name, _)| *name)
        .collect()
}

/// A function match spec — the `MatchSpec` argument of `erlang:trace_pattern/2,3`.
///
/// The removal forms (`false`, `[]`) never reach here: the BIF layer maps them to "no entry",
/// which is also how `install`/`remove` are distinguished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchSpec {
    /// The atom `true` — a catch-all: every call matches.
    MatchAll,
    /// A non-empty list of match functions, retained **verbatim** (never compiled here).
    ///
    /// The words stay valid for the runtime's life (see the module docs); a future match-spec
    /// compiler is what turns them into something a per-call hook can evaluate.
    Functions(Vec<Eterm>),
}

/// One installed function trace pattern: the flags and match spec of a `{M,F,A}` pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternEntry {
    /// The `trace_pattern` flags the pattern was installed with.
    pub flags: PatternFlags,
    /// The installed match spec.
    pub match_spec: MatchSpec,
}

/// A `trace_pattern` `{Module, Function, Arity}` pattern.
///
/// Each selector is either **concrete** (`Some(..)`) or the `'_'` wildcard (`None`, matching
/// anything). All eight combinations are valid, exactly as the oracle accepts
/// `{M,F,A}`/`{'_',F,A}`/`{M,'_',A}`/`{M,F,'_'}` and their wildcard mixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MfaPattern {
    /// The module selector (`None` = `'_'`: every module).
    pub module: Option<String>,
    /// The function selector (`None` = `'_'`: every function).
    pub function: Option<String>,
    /// The arity selector (`None` = `'_'`: every arity).
    pub arity: Option<u32>,
}

impl MfaPattern {
    /// Whether this pattern selects the concrete `module:function/arity` — the predicate a
    /// future `beam_engine` per-call hook consults before emitting a trace event.
    pub fn matches(&self, module: &str, function: &str, arity: u32) -> bool {
        self.module
            .as_deref()
            .is_none_or(|selector| selector == module)
            && self
                .function
                .as_deref()
                .is_none_or(|selector| selector == function)
            && self.arity.is_none_or(|selector| selector == arity)
    }

    /// How many of the three selectors are concrete (`0..=3`) — the pattern's specificity.
    pub fn specificity(&self) -> usize {
        usize::from(self.module.is_some())
            + usize::from(self.function.is_some())
            + usize::from(self.arity.is_some())
    }

    /// The number of exported functions in `modules` this pattern matches.
    ///
    /// `modules` yields `(module_name, functions)` where `functions` yields
    /// `(function_name, arity)` — the shape a caller reads off `beam_loader`'s loaded modules.
    /// This is the number `erlang:trace_pattern/2,3` returns ("the number of functions
    /// matched"), so it is computed from the **argument** pattern alone: it depends neither on
    /// what is already installed nor on whether this call installs or removes.
    pub fn count_matches<'a, M, F>(&self, modules: M) -> usize
    where
        M: IntoIterator<Item = (&'a str, F)>,
        F: IntoIterator<Item = (&'a str, u32)>,
    {
        let mut count = 0usize;
        for (module, functions) in modules {
            for (function, arity) in functions {
                if self.matches(module, function, arity) {
                    count += 1;
                }
            }
        }
        count
    }
}

/// The installed function trace patterns: one `(pattern, entry)` pair per
/// `erlang:trace_pattern/2,3` call, in installation order.
///
/// It is **pure** — no runtime borrow, no terms beyond the raw match-spec words the caller
/// handed in — so it is unit-testable on its own. The BIF half
/// (`erts_internal:trace_pattern/3`) lives with the other trace BIFs.
#[derive(Debug, Default)]
pub struct PatternTable {
    entries: Vec<(MfaPattern, PatternEntry)>,
}

impl PatternTable {
    /// An empty table.
    pub fn new() -> PatternTable {
        PatternTable::default()
    }

    /// Installs `pattern` with `match_spec` and `flags`.
    ///
    /// An already-installed **identical** pattern is replaced in place (keeping its position);
    /// a different pattern is appended. Replacing is what makes a re-install (`true`, then a
    /// `[match_spec()]`) an update rather than a second, competing pattern.
    pub fn install(&mut self, pattern: MfaPattern, match_spec: MatchSpec, flags: PatternFlags) {
        let entry = PatternEntry { flags, match_spec };
        match self.entries.iter_mut().find(|(p, _)| *p == pattern) {
            Some((_, slot)) => *slot = entry,
            None => self.entries.push((pattern, entry)),
        }
    }

    /// Removes `pattern` (`false`/`[]` in a `trace_pattern` call); reports whether it was
    /// installed. Removing a pattern that is not installed is a no-op.
    pub fn remove(&mut self, pattern: &MfaPattern) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(installed, _)| installed != pattern);
        self.entries.len() != before
    }

    /// The entry governing the concrete `module:function/arity`, if any installed pattern
    /// matches it.
    ///
    /// When several patterns match, the **most specific** one wins (the one with the fewest
    /// wildcards — the C's own precedence, so a `{M,F,A}` install overrides a `{'_',F,A}`
    /// one); among equally specific matches the **most recently installed** wins.
    pub fn matches(&self, module: &str, function: &str, arity: u32) -> Option<&PatternEntry> {
        self.entries
            .iter()
            .filter(|(pattern, _)| pattern.matches(module, function, arity))
            .max_by_key(|(pattern, _)| pattern.specificity())
            .map(|(_, entry)| entry)
    }

    /// Whether `pattern` is installed.
    pub fn contains(&self, pattern: &MfaPattern) -> bool {
        self.entries.iter().any(|(installed, _)| installed == pattern)
    }

    /// Every installed pattern, in installation order.
    pub fn patterns(&self) -> impl Iterator<Item = &MfaPattern> {
        self.entries.iter().map(|(pattern, _)| pattern)
    }

    /// The number of installed patterns.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no pattern is installed.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops every installed pattern (the run-time's own `clear_all`).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// One traced process: its flags and the tracer it reports to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceEntry {
    /// The trace flags set on the process.
    pub flags: TraceFlags,
    /// The process the tracee reports to, if a tracer was recorded.
    pub tracer: Option<ProcessId>,
}

/// The node-global trace state: one [`TraceEntry`] per traced process, the installed
/// **function** trace patterns ([`PatternTable`]), plus the monotonic delivery counter behind
/// `erlang:trace_delivered/1`.
///
/// It is **pure** — no runtime borrow — so it is unit-testable on its own.
#[derive(Debug, Default)]
pub struct TraceState {
    entries: HashMap<ProcessId, TraceEntry>,
    patterns: PatternTable,
    clock: u64,
}

impl TraceState {
    /// An empty trace state.
    pub fn new() -> TraceState {
        TraceState::default()
    }

    /// The installed function trace patterns (`erlang:trace_pattern/2,3`).
    pub fn patterns(&self) -> &PatternTable {
        &self.patterns
    }

    /// The installed function trace patterns, mutably — what
    /// `erts_internal:trace_pattern/3` installs into or removes from.
    pub fn patterns_mut(&mut self) -> &mut PatternTable {
        &mut self.patterns
    }
    /// The mutable entry for `pid`, inserting a default (no flags, no tracer) if absent.
    ///
    /// The get-or-insert half of [`TraceState::set`]: a caller that only wants to *change* one
    /// field of an existing entry can use this, and a fresh pid gets an empty entry.
    pub fn entry(&mut self, pid: ProcessId) -> &mut TraceEntry {
        self.entries.entry(pid).or_default()
    }

    /// The entry for `pid`, or `None` if the process is not traced.
    pub fn get(&self, pid: ProcessId) -> Option<&TraceEntry> {
        self.entries.get(&pid)
    }

    /// Records `pid` as traced with `flags` reported to `tracer` (replacing any prior entry).
    pub fn set(&mut self, pid: ProcessId, flags: TraceFlags, tracer: Option<ProcessId>) {
        self.entries.insert(pid, TraceEntry { flags, tracer });
    }

    /// Stops tracing `pid` (a no-op for an untraced one).
    pub fn clear(&mut self, pid: ProcessId) {
        self.entries.remove(&pid);
    }

    /// Whether `pid` has a trace entry.
    pub fn is_traced(&self, pid: ProcessId) -> bool {
        self.entries.contains_key(&pid)
    }

    /// Every traced pid, in no particular order.
    pub fn traced_pids(&self) -> Vec<ProcessId> {
        self.entries.keys().copied().collect()
    }

    /// The next value of the monotonic trace-delivery counter.
    ///
    /// It starts at 1 and never repeats, which is what gives `erlang:trace_delivered/1` a
    /// stable delivery sequence. The counter is **not** reset by [`TraceState::clear_all`], so
    /// it stays monotonic for the runtime's lifetime.
    pub fn next_ref_seq(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// The current delivery counter (the last value [`TraceState::next_ref_seq`] returned).
    pub fn ref_seq(&self) -> u64 {
        self.clock
    }

    /// Stops tracing every process (the counter is left monotonic).
    pub fn clear_all(&mut self) {
        self.entries.clear();
    }
}

// The BIF layer -----------------------------------------------------------------------------

impl RuntimeContext<'_> {
    /// `erlang:trace_info(Pid, Item)` — the C's `trace_info_2` (`erl_bif_trace.c`).
    ///
    /// | `Item` | Answer (live untraced pid / traced pid) |
    /// |---|---|
    /// | `flags` | `{flags, [Atom, …]}` — the canonical decode order; `[]` when no flag is set |
    /// | `tracer` | `{tracer, Tracer}` when a tracer is recorded, else `{tracer, []}` |
    /// | `match_spec` | `{match_spec, []}` — a **process** match spec, which
    ///   `erlang:trace_pattern/2,3` (a *function*-level facility, see [`PatternTable`]) never
    ///   sets; the runtime has no other writer for it, so it is always `[]` |
    /// | `traced` | `{traced, true \| false}` |
    /// | `all` | the list of all four `{Item, Value}` pairs above, in that order |
    ///
    /// A **dead/unknown** pid answers the atom `undefined` (checked before the item is
    /// dispatched, exactly as the C checks `erts_pid2proc_opt` before its item switch); an
    /// **unknown** item atom, or a first argument that is not an internal pid, is `badarg`.
    ///
    /// Divergence (documented): the OTP-29.1.1 oracle answers `{error, badarg}` for the `all`
    /// item; OTP-30 returns the list of pairs, which is what this implements.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for the argument errors above, and whatever the term writers
    /// report (a full heap).
    pub(crate) fn trace_info_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let item_term = *args.get(1).ok_or(BeamError::BadArg)?;
        // The item must be an atom: a non-atom is `badarg` (the C's `is_atom` gate).
        let item = self.atom_name(item_term).ok_or(BeamError::BadArg)?;
        let pid_term = *args.first().ok_or(BeamError::BadArg)?;
        // Only an internal pid is a valid tracee; an MFA/function spec is not modelled.
        let pid = ProcessId::from_eterm(pid_term).map_err(|_| BeamError::BadArg)?;
        // A dead/unknown process answers `undefined` — before the item is dispatched.
        if !self.process_table().contains(pid) {
            return self.atom("undefined");
        }
        let entry = self.runtime.trace().get(pid).cloned();
        match item.as_str() {
            "flags" => {
                let flags = entry.as_ref().map(|e| e.flags).unwrap_or_default();
                let list = self.trace_flags_list(flags)?;
                let tag = self.atom("flags")?;
                terms::make_tuple(self, &[tag, list])
            }
            "tracer" => {
                let value = match entry.as_ref().and_then(|e| e.tracer) {
                    Some(tracer) => tracer.to_eterm(),
                    None => NIL,
                };
                let tag = self.atom("tracer")?;
                terms::make_tuple(self, &[tag, value])
            }
            "match_spec" => {
                let tag = self.atom("match_spec")?;
                terms::make_tuple(self, &[tag, NIL])
            }
            "traced" => {
                let value = if entry.is_some() {
                    self.atom("true")?
                } else {
                    self.atom("false")?
                };
                let tag = self.atom("traced")?;
                terms::make_tuple(self, &[tag, value])
            }
            "all" => self.trace_info_all(entry),
            _ => Err(BeamError::BadArg),
        }
    }

    /// `erlang:trace_delivered(Tracee)` — a delivery barrier for `Tracee`.
    ///
    /// `Tracee` must be a pid the process table holds, or the atom `all`; anything else is
    /// `badarg`. A fresh reference is minted from the runtime's one ref id space, the message
    /// `{trace_delivered, Tracee, Ref}` is delivered to the **calling** process (delivery is
    /// synchronous in this single-threaded runtime) and `Ref` is returned.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a bad `Tracee`, and whatever the ref/message writers report.
    pub(crate) fn trace_delivered_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let target = *args.first().ok_or(BeamError::BadArg)?;
        let is_all = self.atom_name(target).as_deref() == Some("all");
        if !is_all {
            let pid = ProcessId::from_eterm(target).map_err(|_| BeamError::BadArg)?;
            if !self.process_table().contains(pid) {
                return Err(BeamError::BadArg);
            }
        }
        // A monotonic delivery sequence on the trace state; the reference *id* itself comes from
        // the runtime's one ref id space (the same space `make_ref/0` and monitors draw from).
        let _seq = self.runtime.trace_mut().next_ref_seq();
        let reference = self.runtime.refs_mut().next();
        let mut heap = self.runtime.take_bif_heap()?;
        let ref_term = reference.to_eterm(&mut heap);
        self.runtime.put_bif_heap(heap);
        let ref_term = ref_term?;
        let tag = self.atom("trace_delivered")?;
        let message = terms::make_tuple(self, &[tag, target, ref_term])?;
        self.send_message(self.pid().to_eterm(), message)?;
        Ok(ref_term)
    }

    /// The `{flags, [Atom, …]}` value of [`RuntimeContext::trace_info_2`]'s `flags` item: the
    /// canonical atom list for `flags`, materialized on the engine's heap seam.
    fn trace_flags_list(&mut self, flags: TraceFlags) -> Result<Eterm> {
        let names = flags_to_atoms(flags);
        let mut elems = Vec::with_capacity(names.len());
        for name in names {
            elems.push(self.atom(name)?);
        }
        terms::make_list(self, &elems)
    }

    /// The `all` item of [`RuntimeContext::trace_info_2`]: the four `{Item, Value}` pairs
    /// `flags`, `tracer`, `match_spec`, `traced`, as a list.
    fn trace_info_all(&mut self, entry: Option<TraceEntry>) -> Result<Eterm> {
        let flags = entry.as_ref().map(|e| e.flags).unwrap_or_default();
        let tracer = entry.as_ref().and_then(|e| e.tracer);
        let traced = entry.is_some();

        let flags_tag = self.atom("flags")?;
        let flags_list = self.trace_flags_list(flags)?;
        let flags_pair = terms::make_tuple(self, &[flags_tag, flags_list])?;

        let tracer_tag = self.atom("tracer")?;
        let tracer_value = match tracer {
            Some(tracer) => tracer.to_eterm(),
            None => NIL,
        };
        let tracer_pair = terms::make_tuple(self, &[tracer_tag, tracer_value])?;

        let match_spec_tag = self.atom("match_spec")?;
        let match_spec_pair = terms::make_tuple(self, &[match_spec_tag, NIL])?;

        let traced_tag = self.atom("traced")?;
        let traced_value = if traced {
            self.atom("true")?
        } else {
            self.atom("false")?
        };
        let traced_pair = terms::make_tuple(self, &[traced_tag, traced_value])?;

        terms::make_list(
            self,
            &[flags_pair, tracer_pair, match_spec_pair, traced_pair],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact canonical decode order the oracle reports (OTP 29.1.1 / erts-17.1).
    const EXPECTED_ORDER: &[&str] = &[
        "running_procs",
        "running_ports",
        "ports",
        "scheduler_id",
        "silent",
        "return_to",
        "arity",
        "garbage_collection",
        "exiting",
        "running",
        "set_on_first_link",
        "set_on_link",
        "set_on_first_spawn",
        "procs",
        "call",
        "set_on_spawn",
        "receive",
        "send",
        "monotonic_timestamp",
        "strict_monotonic_timestamp",
        "timestamp",
    ];

    #[test]
    fn a_single_flag_round_trips_through_its_atom() {
        let flags = flags_from_atoms(&["timestamp"]).expect("timestamp");
        assert_eq!(flags_to_atoms(flags), vec!["timestamp"]);
    }

    #[test]
    fn repeated_atoms_are_deduplicated() {
        let once = flags_from_atoms(&["send"]).expect("send");
        let twice = flags_from_atoms(&["send", "send"]).expect("send twice");
        assert_eq!(once, twice);
        assert_eq!(flags_to_atoms(twice), vec!["send"]);
    }

    #[test]
    fn an_unknown_atom_is_rejected() {
        assert!(flags_from_atoms(&["not_a_trace_flag"]).is_none());
        // A single unknown atom poisons the whole decode, even beside a valid one.
        assert!(flags_from_atoms(&["send", "not_a_trace_flag"]).is_none());
        // An empty list is zero flags, not an error.
        assert_eq!(flags_from_atoms(&[]), Some(TraceFlags(0)));
    }

    #[test]
    fn all_expands_to_every_flag_in_canonical_order() {
        let all = flags_from_atoms(&["all"]).expect("all");
        assert_eq!(flags_to_atoms(all), EXPECTED_ORDER);
        // `all` beside another flag is still just `all`.
        let all_plus = flags_from_atoms(&["all", "send"]).expect("all + send");
        assert_eq!(all_plus, all);
        // Every accepted atom is a subset of `all` and is reported without the extra atoms.
        for &(name, _) in CANONICAL {
            let flag = flags_from_atoms(&[name]).expect("a canonical atom");
            assert_eq!(flags_to_atoms(flag), vec![name]);
            // The flag is a subset of `all`.
            assert_eq!(
                flags_to_atoms(TraceFlags(flag.0 | all.0)),
                EXPECTED_ORDER,
                "{name} is outside `all`"
            );
        }
    }

    #[test]
    fn atoms_without_a_canonical_atom_are_accepted_but_not_decoded() {
        // `return_from` and `cpu_timestamp` are real flags the runtime must accept, but the
        // oracle's `trace_info(P, flags)` never lists them.
        for name in ["return_from", "cpu_timestamp"] {
            let flag = flags_from_atoms(&[name]).expect("an accepted atom");
            assert_eq!(
                flags_to_atoms(flag),
                Vec::<&str>::new(),
                "{name} must decode to no canonical atom"
            );
        }
    }

    #[test]
    fn the_canonical_order_is_exactly_the_oracles() {
        let all = flags_from_atoms(&["all"]).expect("all");
        let mut got = flags_to_atoms(all);
        // A reorder of `EXPECTED_ORDER` would show up as a mismatch here.
        assert_eq!(got, EXPECTED_ORDER.to_vec());
        got.reverse();
        assert_ne!(got, EXPECTED_ORDER.to_vec());
    }

    // The state model -----------------------------------------------------------------------

    fn pid(n: u64) -> ProcessId {
        ProcessId::new(n, 0)
    }

    #[test]
    fn set_get_and_clear_are_a_real_registry() {
        let mut state = TraceState::new();
        let flags = flags_from_atoms(&["call", "return_to"]).expect("flags");
        assert!(!state.is_traced(pid(1)));
        assert_eq!(state.get(pid(1)), None);

        state.set(pid(1), flags, Some(pid(2)));
        assert!(state.is_traced(pid(1)));
        let entry = state.get(pid(1)).expect("the entry");
        assert_eq!(entry.flags, flags);
        assert_eq!(entry.tracer, Some(pid(2)));

        // A second `set` replaces the entry wholesale.
        state.set(pid(1), TraceFlags(0), None);
        assert_eq!(state.get(pid(1)).expect("the entry").tracer, None);

        assert_eq!(state.traced_pids(), vec![pid(1)]);
        state.clear(pid(1));
        assert!(!state.is_traced(pid(1)));
        // Clearing an untraced pid is a no-op.
        state.clear(pid(9));
        assert!(state.traced_pids().is_empty());
    }

    #[test]
    fn entry_inserts_a_default_entry() {
        let mut state = TraceState::new();
        assert_eq!(*state.entry(pid(3)), TraceEntry::default());
        state.entry(pid(3)).flags = flags_from_atoms(&["send"]).expect("send");
        assert_eq!(
            state.get(pid(3)).expect("the entry").flags,
            flags_from_atoms(&["send"]).unwrap()
        );
    }

    #[test]
    fn the_delivery_counter_is_monotonic_and_survives_clear_all() {
        let mut state = TraceState::new();
        assert_eq!(state.ref_seq(), 0);
        assert_eq!(state.next_ref_seq(), 1);
        assert_eq!(state.next_ref_seq(), 2);
        assert_eq!(state.ref_seq(), 2);

        state.set(pid(1), TraceFlags(0), None);
        state.clear_all();
        assert!(!state.is_traced(pid(1)));
        assert!(state.traced_pids().is_empty());
        // The counter is not reset by `clear_all`.
        assert_eq!(state.ref_seq(), 2);
        assert_eq!(state.next_ref_seq(), 3);
    }

    // The trace-pattern model ---------------------------------------------------------------

    /// A pattern from its three selectors (`None` = the `'_'` wildcard).
    fn pattern(module: Option<&str>, function: Option<&str>, arity: Option<u32>) -> MfaPattern {
        MfaPattern {
            module: module.map(str::to_string),
            function: function.map(str::to_string),
            arity,
        }
    }

    /// The `(module, [(function, arity)])` shape [`MfaPattern::count_matches`] walks.
    type Modules<'a> = [(&'a str, Vec<(&'a str, u32)>)];

    /// `pattern.count_matches(modules)` over the test-shaped modules list.
    fn count(pattern: &MfaPattern, modules: &Modules<'_>) -> usize {
        pattern.count_matches(
            modules
                .iter()
                .map(|(module, functions)| (*module, functions.iter().map(|(f, a)| (*f, *a)))),
        )
    }

    /// A module list mirroring the `slice` fixture plus a second module, so a wildcard count
    /// is not accidentally a "one module" count.
    fn modules() -> Vec<(&'static str, Vec<(&'static str, u32)>)> {
        vec![
            (
                "slice",
                vec![
                    ("add", 2),
                    ("fib", 1),
                    ("pick", 1),
                    ("module_info", 0),
                    ("module_info", 1),
                ],
            ),
            ("other", vec![("add", 3), ("fib", 1)]),
        ]
    }

    #[test]
    fn the_pattern_flags_are_exactly_the_seven_valid_atoms() {
        let all = pattern_flags_from_atoms(&[
            "local",
            "global",
            "meta",
            "call_count",
            "call_time",
            "call_memory",
            "silent",
        ])
        .expect("every valid atom");
        assert_eq!(
            pattern_flags_to_atoms(all),
            vec![
                "local",
                "global",
                "meta",
                "call_count",
                "call_time",
                "call_memory",
                "silent"
            ]
        );
        // Every single atom decodes to exactly itself.
        for &(name, _) in PATTERN_CANONICAL {
            let flags = pattern_flags_from_atoms(&[name]).expect("a valid atom");
            assert_eq!(pattern_flags_to_atoms(flags), vec![name]);
        }
        // An empty list is zero flags, not an error.
        assert_eq!(pattern_flags_from_atoms(&[]), Some(PatternFlags(0)));
        assert!(pattern_flags_to_atoms(PatternFlags(0)).is_empty());
    }

    #[test]
    fn a_process_trace_flag_is_not_a_trace_pattern_flag() {
        // `call` is the oracle-observed rejection: it is a *process* flag, so `[call]` is
        // `badarg` here even though `flags_from_atoms(["call"])` is fine.
        assert!(flags_from_atoms(&["call"]).is_some());
        assert!(pattern_flags_from_atoms(&["call"]).is_none());
        assert!(pattern_flags_from_atoms(&["local", "call"]).is_none());
        assert!(pattern_flags_from_atoms(&["not_a_pattern_flag"]).is_none());
    }

    #[test]
    fn a_concrete_pattern_matches_only_its_own_mfa() {
        let p = pattern(Some("slice"), Some("add"), Some(2));
        assert!(p.matches("slice", "add", 2));
        assert!(!p.matches("slice", "add", 3));
        assert!(!p.matches("slice", "fib", 2));
        assert!(!p.matches("other", "add", 2));
        assert_eq!(p.specificity(), 3);
    }

    #[test]
    fn every_wildcard_shape_matches_the_documented_set() {
        // `{'_', F, A}`: every module, one function/arity.
        let by_function = pattern(None, Some("add"), Some(2));
        assert!(by_function.matches("slice", "add", 2));
        assert!(by_function.matches("other", "add", 2));
        assert!(!by_function.matches("other", "add", 3));
        assert_eq!(by_function.specificity(), 2);

        // `{M, '_', A}`: one module, every function of that arity.
        let by_arity = pattern(Some("slice"), None, Some(1));
        assert!(by_arity.matches("slice", "fib", 1));
        assert!(by_arity.matches("slice", "pick", 1));
        assert!(!by_arity.matches("slice", "add", 2));
        assert!(!by_arity.matches("other", "fib", 1));

        // `{M, F, '_'}`: one module/function, every arity.
        let by_name = pattern(Some("slice"), Some("module_info"), None);
        assert!(by_name.matches("slice", "module_info", 0));
        assert!(by_name.matches("slice", "module_info", 1));
        assert!(!by_name.matches("slice", "add", 2));

        // The full wildcard matches everything.
        let all = pattern(None, None, None);
        assert!(all.matches("anything", "at_all", 17));
        assert_eq!(all.specificity(), 0);
    }

    #[test]
    fn count_matches_counts_exports_for_each_pattern_shape() {
        let modules = modules();
        // The concrete pattern matches slice's `add/2` only — not `other`'s `add/3`.
        assert_eq!(count(&pattern(Some("slice"), Some("add"), Some(2)), &modules), 1);
        assert_eq!(count(&pattern(Some("other"), Some("add"), Some(3)), &modules), 1);
        // A module/function/arity that does not exist is `0`, never an error.
        assert_eq!(count(&pattern(Some("slice"), Some("add"), Some(9)), &modules), 0);
        assert_eq!(count(&pattern(Some("absent"), Some("add"), Some(2)), &modules), 0);
        // `{'_', add, 2}` is slice's `add/2` only (other's `add` is arity 3).
        assert_eq!(count(&pattern(None, Some("add"), Some(2)), &modules), 1);
        // `{'_', fib, 1}` is both modules' `fib/1`.
        assert_eq!(count(&pattern(None, Some("fib"), Some(1)), &modules), 2);
        // `{slice, '_', 1}` is slice's three arity-1 exports.
        assert_eq!(count(&pattern(Some("slice"), None, Some(1)), &modules), 3);
        // `{slice, module_info, '_'}` is both of slice's `module_info` exports.
        assert_eq!(count(&pattern(Some("slice"), Some("module_info"), None), &modules), 2);
        // The full wildcard is every export of every module.
        assert_eq!(count(&pattern(None, None, None), &modules), 7);
    }

    #[test]
    fn install_replace_and_remove_are_a_real_registry() {
        let mut table = PatternTable::new();
        assert!(table.is_empty());
        assert_eq!(table.matches("slice", "add", 2), None);

        let p = pattern(Some("slice"), Some("add"), Some(2));
        let local = pattern_flags_from_atoms(&["local"]).expect("local");
        table.install(p.clone(), MatchSpec::MatchAll, local);
        assert_eq!(table.len(), 1);
        assert!(table.contains(&p));
        let entry = table.matches("slice", "add", 2).expect("the installation");
        assert_eq!(entry.flags, local);
        assert_eq!(entry.match_spec, MatchSpec::MatchAll);

        // Re-installing the *same* pattern updates in place (one entry, the new values).
        let global = pattern_flags_from_atoms(&["global"]).expect("global");
        table.install(p.clone(), MatchSpec::Functions(vec![beam_term::make_small(7)]), global);
        assert_eq!(table.len(), 1, "an identical pattern is replaced, not duplicated");
        let entry = table.matches("slice", "add", 2).expect("still installed");
        assert_eq!(entry.flags, global);
        assert_eq!(entry.match_spec, MatchSpec::Functions(vec![beam_term::make_small(7)]));

        // A different pattern is a second entry.
        let other = pattern(Some("slice"), Some("fib"), Some(1));
        table.install(other.clone(), MatchSpec::MatchAll, local);
        assert_eq!(table.len(), 2);
        assert_eq!(table.patterns().count(), 2);

        // Removing reports whether the pattern was there; removing the *other* pattern leaves
        // this one untouched.
        assert!(table.remove(&other));
        assert!(!table.remove(&other), "a second removal is a no-op");
        assert_eq!(table.len(), 1);
        assert!(table.contains(&p));
        assert!(table.remove(&p));
        assert!(table.is_empty());
        assert_eq!(table.matches("slice", "add", 2), None);
    }

    #[test]
    fn a_wildcard_and_a_specific_pattern_coexist_and_the_specific_one_wins() {
        let local = pattern_flags_from_atoms(&["local"]).expect("local");
        let silent = pattern_flags_from_atoms(&["silent"]).expect("silent");
        let mut table = PatternTable::new();

        // Installed first, most general: `{'_', add, 2}`.
        table.install(pattern(None, Some("add"), Some(2)), MatchSpec::MatchAll, local);
        // Installed second, most specific: `{slice, add, 2}`.
        table.install(
            pattern(Some("slice"), Some("add"), Some(2)),
            MatchSpec::MatchAll,
            silent,
        );
        // The more specific pattern governs slice's `add/2`, the general one governs `other`'s.
        assert_eq!(
            table.matches("slice", "add", 2).expect("matched").flags,
            silent
        );
        assert_eq!(table.matches("other", "add", 2).expect("matched").flags, local);
        // The specificity rule holds whichever order they were installed in.
        let mut reversed = PatternTable::new();
        reversed.install(
            pattern(Some("slice"), Some("add"), Some(2)),
            MatchSpec::MatchAll,
            silent,
        );
        reversed.install(pattern(None, Some("add"), Some(2)), MatchSpec::MatchAll, local);
        assert_eq!(
            reversed.matches("slice", "add", 2).expect("matched").flags,
            silent
        );
        // Removing the specific pattern falls back to the general one.
        assert!(reversed.remove(&pattern(Some("slice"), Some("add"), Some(2))));
        assert_eq!(reversed.matches("slice", "add", 2).expect("matched").flags, local);
        assert_eq!(reversed.matches("slice", "add", 9), None, "arity still selected");
    }

    #[test]
    fn the_trace_state_owns_the_pattern_table() {
        let mut state = TraceState::new();
        assert!(state.patterns().is_empty());
        let p = pattern(Some("slice"), Some("add"), Some(2));
        state.patterns_mut().install(
            p.clone(),
            MatchSpec::MatchAll,
            pattern_flags_from_atoms(&["local"]).expect("local"),
        );
        assert!(state.patterns().contains(&p));
        // `clear_all` is about *processes*: the installed patterns survive it.
        state.clear_all();
        assert!(state.patterns().contains(&p));
        state.patterns_mut().clear();
        assert!(state.patterns().is_empty());
    }
}
