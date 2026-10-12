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
//! * the **`send` and `'receive'` events** of the ordinary trace surface:
//!   [`RuntimeContext::trace_send_events`] and [`RuntimeContext::trace_receive_events`] are
//!   called by the send seam (the `RuntimeContext::send_message` the engine's `send`
//!   instruction reaches). The first, after the payload has been delivered, emits `{trace,
//!   Sender, send, Msg, To}` — or `{trace_ts, …}` with the `timestamp` family's trailing
//!   element — to the **sending** process's tracer, plus the `{seq_trace, Label, {send, …}}`
//!   event when the sender's sequential-trace token has the `send` flag set; the second emits
//!   `{trace, Receiver, 'receive', Msg}` to the **receiving** process's tracer when that
//!   process's `'receive'` flag is set, on **enqueue**;
//! * `erts_internal:trace_pattern/3` (the body `erlang:trace_pattern/2,3` calls) — the
//!   **function** trace patterns. [`TraceState`] owns a [`PatternTable`]: one entry per
//!   installed `{M,F,A}` pattern (wildcard-aware — [`MfaPattern::matches`]) holding its
//!   [`PatternFlags`] and its [`MatchSpec`]. The BIF's answer is the number of **exported**
//!   functions of the modules this runtime has loaded that the pattern matches
//!   ([`MfaPattern::count_matches`]).
//!
//! # Deliberately out of scope (a later stage)
//!
//! * **The remaining event families.** Two events are emitted, both from the send seam: the
//!   `send` event ([`RuntimeContext::trace_send_events`]) and the delivered-message
//!   `{trace, Receiver, 'receive', Msg}` event ([`RuntimeContext::trace_receive_events`],
//!   emitted on **enqueue** and gated on the *receiver's* flag). The `spawn`, `exit`,
//!   `register`/`unregister`, `in`/`out` and `call`/`return_to`/`return_from` events have no
//!   producer — nothing is wired into the engine's spawn/exit or call paths, so those flags
//!   are state only (deliberately unemitted rather than emitted with an unpinned shape).
//!   [`PatternTable::matches`] is the predicate a future `beam_engine` per-call hook calls to
//!   decide whether a `call`/`return_from`/`return_to` event should be emitted; until that hook
//!   exists it is only a query. Nor does a **match spec** filter the emitted events (none is
//!   compiled — see below).
//! * **Carrying the sequential-trace token with a message.** The `{send, …}` event is emitted,
//!   but the token is *not* attached to the message the send delivers, so a receiver cannot
//!   inherit it (that needs a per-message slot in `beam_message`) and no `{'receive', …}`
//!   sequential-trace event can follow.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_process::ProcessId;
use beam_term::{make_small, Eterm, NIL};

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

impl TraceFlags {
    /// Whether the `call` process flag is set — the gate for the `call`/`return_from` event
    /// family and the precondition for arming `return_to`.
    pub(crate) fn has_calls(self) -> bool {
        self.0 & CALL != 0
    }

    /// Whether the `return_to` process flag is set.
    pub(crate) fn has_return_to(self) -> bool {
        self.0 & RETURN_TO != 0
    }

    /// Whether the `arity` process flag is set (the `call` event carries the bare arity).
    pub(crate) fn has_arity(self) -> bool {
        self.0 & ARITY != 0
    }
}

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

impl PatternFlags {
    /// Whether the `local` `trace_pattern` flag is set.
    pub(crate) fn is_local(self) -> bool {
        self.0 & PATTERN_LOCAL != 0
    }

    /// Whether the `global` `trace_pattern` flag is set.
    pub(crate) fn is_global(self) -> bool {
        self.0 & PATTERN_GLOBAL != 0
    }

    /// Whether the `silent` `trace_pattern` flag is set.
    pub(crate) fn is_silent(self) -> bool {
        self.0 & PATTERN_SILENT != 0
    }
}

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

// Event time stamps -------------------------------------------------------------------------

/// The time-stamp flavour an emitted event carries, selected from the **sending** process's
/// trace flags.
///
/// Whichever flag is set is the flavour; the oracle's precedence is `timestamp` >
/// `strict_monotonic_timestamp` > `monotonic_timestamp`. A flavour makes the event tag
/// `trace_ts` instead of `trace` and appends one trailing element (see
/// [`RuntimeContext::emit_trace_event`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TraceTimestamp {
    /// `timestamp`: the wall-clock `{MegaSecs, Secs, MicroSecs}` of `erlang:timestamp/0`.
    Wall,
    /// `monotonic_timestamp`: the runtime's monotonic clock, in native (nanosecond) units —
    /// the same unit `erlang:monotonic_time/0` reports.
    Monotonic,
    /// `strict_monotonic_timestamp`: `{Nanoseconds, UniqueMonotonicInteger}`, unique across
    /// the events this runtime emits.
    StrictMonotonic,
}

impl TraceTimestamp {
    /// The flavour `flags` selects, or `None` when no time-stamp flag is set.
    pub(crate) fn select(flags: TraceFlags) -> Option<TraceTimestamp> {
        if flags.0 & TIMESTAMP != 0 {
            Some(TraceTimestamp::Wall)
        } else if flags.0 & STRICT_MONOTONIC_TIMESTAMP != 0 {
            Some(TraceTimestamp::StrictMonotonic)
        } else if flags.0 & MONOTONIC_TIMESTAMP != 0 {
            Some(TraceTimestamp::Monotonic)
        } else {
            None
        }
    }

    /// The event tag this flavour produces (`erlang:trace/3`: a time-stamped event is a
    /// `trace_ts`, whichever of the three flags asked for it).
    pub(crate) fn tag(self) -> &'static str {
        "trace_ts"
    }
}

/// The `{MegaSecs, Secs, MicroSecs}` triple of `erlang:timestamp/0` for a duration since the
/// Unix epoch.
///
/// The seconds are split into a millions part and a remainder exactly as the `os:timestamp/0`
/// triple is defined, so `MegaSecs` counts whole millions of seconds and `Secs` is the rest.
fn wall_clock_triple(since_epoch: Duration) -> (i64, i64, i64) {
    let seconds = since_epoch.as_secs();
    (
        (seconds / 1_000_000) as i64,
        (seconds % 1_000_000) as i64,
        i64::from(since_epoch.subsec_micros()),
    )
}

/// The `timestamp` flavour's element for the wall clock read now.
///
/// A clock before the Unix epoch (which no supported platform reports) is the epoch itself —
/// never a panic and never a fabricated "now".
fn wall_clock_now() -> (i64, i64, i64) {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since_epoch) => wall_clock_triple(since_epoch),
        Err(_) => wall_clock_triple(Duration::ZERO),
    }
}

/// The monotonic base of this runtime: the instant the first monotonic time stamp was asked
/// for.
///
/// `timers.rs` keeps no exported epoch (its deadlines are [`Instant`]s compared against each
/// other), so the base is established here on first use. `Instant` is the platform's monotonic
/// clock, so the difference never goes backwards and is immune to wall-clock adjustments.
static MONOTONIC_BASE: OnceLock<Instant> = OnceLock::new();

/// Nanoseconds elapsed since [`MONOTONIC_BASE`] — `erlang:monotonic_time()`'s native unit.
fn monotonic_nanos() -> i64 {
    let base = *MONOTONIC_BASE.get_or_init(Instant::now);
    elapsed_nanos(Instant::now().duration_since(base))
}

/// A duration in nanoseconds as the integer `erlang:monotonic_time/0` reports (`i64`, which
/// covers the full `Instant` range — the saturation is the C's own `i64` counter).
fn elapsed_nanos(elapsed: Duration) -> i64 {
    elapsed.as_nanos().min(i64::MAX as u128) as i64
}

/// The `UniqueMonotonicInteger` of a `strict_monotonic_timestamp`: the next value of a
/// process-wide counter that only ever increases.
static UNIQUE_MONOTONIC: AtomicU64 = AtomicU64::new(0);

/// The next unique monotonic integer (1-based, strictly increasing per runtime process).
fn next_unique_monotonic() -> u64 {
    UNIQUE_MONOTONIC.fetch_add(1, Ordering::Relaxed) + 1
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
        // The reference comes from the runtime's **own** id space (the one `make_ref/0` and the
        // timers draw from) — not the process table's monitor/alias space. This `trace_delivered`
        // reference is a **bare** runtime-minted reference, so record it as such in the provenance
        // registry: that is what tells it apart from a monitor/alias reference and keeps a send to
        // it from ever resolving as an alias (`Runtime::local_reference`).
        let _seq = self.runtime.trace_mut().next_ref_seq();
        let reference = self.runtime.refs_mut().next();
        let mut heap = self.runtime.take_bif_heap()?;
        let ref_term = reference.to_eterm(&mut heap);
        self.runtime.put_bif_heap(heap);
        let ref_term = ref_term?;
        self.runtime.references.insert(ref_term, None);
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

// Emitting events ---------------------------------------------------------------------------

impl RuntimeContext<'_> {
    /// Emits one `{trace, Traced, Event, Payload…}` / `{trace_ts, …}` event message to
    /// `traced`'s tracer — the single term-building and delivery point for the events the
    /// ordinary trace surface produces (contract A.8, item 1).
    ///
    /// * `traced` is the process the event is **about**, and the one whose flags and tracer are
    ///   consulted. For the `send` event that is the **sending** process: the `send` flag lives
    ///   on the sender, so a third party sending *to* a `[send]`-traced process produces no
    ///   event.
    /// * `event` is the event atom (`send`) and `payload` are its elements, in order, after the
    ///   `{trace, Traced, Event, …}` prefix — for a `send` the message and then the destination,
    ///   i.e. the whole event is `{trace, Sender, send, Msg, To}`.
    /// * The event is **delivered as a message** to the tracer ([`TraceEntry::tracer`]) — never
    ///   returned — and delivered through [`RuntimeContext::deliver_message`] rather than the
    ///   `send` seam, so emitting an event does not itself emit an event. (The oracle's own
    ///   trace messages bypass tracing the same way; without the bypass a traced sender whose
    ///   tracer is itself traced would recurse forever.)
    /// * The trailing element is the [`TraceTimestamp::select`] flavour of the sender's flags,
    ///   if any; the tag is then `trace_ts` ([`TraceTimestamp::tag`]).
    ///
    /// Nothing is emitted when `traced` has no trace entry, when its entry records no tracer,
    /// when its `silent` flag is set — a `silent` tracee is the flag that suppresses its
    /// events, and `erts_internal:trace/3` here records a tracer even for it — or when the
    /// tracer **is** the tracee (ERTS's `discard` rule, see below).
    ///    /// # Errors
    ///
    /// Whatever the term writers report (a full heap). A tracer the process table no longer
    /// holds is **not** an error: the event is dropped, exactly as the oracle drops an
    /// undeliverable trace message (tracing must never fail the traced program).
    /// `pub(crate)` because it is the emitter [`RuntimeContext::trace_send_events`] builds on;
    /// the next event family (a `beam_engine` per-call hook) calls it the same way.
    pub(crate) fn emit_trace_event(
        &mut self,
        traced: ProcessId,
        event: &str,
        payload: &[Eterm],
    ) -> Result<()> {
        let Some(entry) = self.runtime.trace().get(traced).cloned() else {
            return Ok(());
        };
        if entry.flags.0 & SILENT != 0 {
            return Ok(());
        }
        let Some(tracer) = entry.tracer else {
            return Ok(());
        };
        // ERTS never delivers an event to a process **about itself**: the built-in tracer
        // answers `discard` when the tracer state *is* the tracee
        // (`erts/emulator/nifs/common/erl_tracer_nif.c:143-146`, "Only generate trace for
        // when tracer != tracee"). A process that traces itself — `erlang:trace(self(), true,
        // [send])` makes the calling process its own default tracer — therefore observes no
        // events of its own; the oracle delivers nothing there.
        if tracer == traced {
            return Ok(());
        }
        let timestamp = TraceTimestamp::select(entry.flags);
        let tag = self.atom(timestamp.map_or("trace", TraceTimestamp::tag))?;
        let event = self.atom(event)?;
        let from = traced.to_eterm();
        let mut elements = Vec::with_capacity(payload.len() + 4);
        elements.push(tag);
        elements.push(from);
        elements.push(event);
        elements.extend_from_slice(payload);
        if let Some(flavour) = timestamp {
            let stamp = self.timestamp_term(flavour)?;
            elements.push(stamp);
        }
        let message = terms::make_tuple(self, &elements)?;
        match self.deliver_message(tracer, from, message) {
            // No process for the tracer: the oracle drops the event and the traced program's
            // send stays successful (`bif.c:2478-2480`).
            Err(BeamError::TermNotFound) => Ok(()),
            delivered => delivered,
        }
    }

    /// The trailing time-stamp element of a `trace_ts` event, for one [`TraceTimestamp`]
    /// flavour: `{MegaSecs, Secs, MicroSecs}` (wall clock), the nanoseconds since the
    /// runtime's monotonic base, or `{Nanoseconds, UniqueMonotonicInteger}`.
    fn timestamp_term(&mut self, flavour: TraceTimestamp) -> Result<Eterm> {
        match flavour {
            TraceTimestamp::Wall => {
                let (mega, secs, micro) = wall_clock_now();
                let elements = [make_small(mega), make_small(secs), make_small(micro)];
                terms::make_tuple(self, &elements)
            }
            TraceTimestamp::Monotonic => Ok(make_small(monotonic_nanos())),
            TraceTimestamp::StrictMonotonic => {
                let nanos = make_small(monotonic_nanos());
                // The unique integer is a `u64` counter; every value this accepts fits the
                // 60-bit small range for any realistic uptime (`u32::MAX` events, at least).
                let unique = make_small(i64::try_from(next_unique_monotonic()).unwrap_or(i64::MAX));
                let elements = [nanos, unique];
                terms::make_tuple(self, &elements)
            }
        }
    }

    /// Emits the trace events a **completed** `send` produces for the sending process — the
    /// runtime's half of contract A.8 item 1. Called by the send seam
    /// ([`RuntimeContext::send_message`](crate::context::RuntimeContext::send_message)) once the
    /// payload has been delivered; the caller is the sending process (`self.pid()`), and `to` is
    /// the destination **term as passed to the send**, which the events carry verbatim.
    ///
    /// Two independent events:
    ///
    /// 1. the ordinary `{trace, Sender, send, Msg, To}` — emitted only when the sender's trace
    ///    entry has the `send` bit set ([`RuntimeContext::emit_trace_event`] does the rest);
    /// 2. the sequential-trace `{seq_trace, Label, {send, Serial, From, To, Msg}}`
    ///    (`lib/kernel/src/seq_trace.erl:62-90`), emitted only when the sender's token has the
    ///    `send` flag ([`crate::seq_trace::FLAG_SEND`]). The token is **stamped first** — the
    ///    documented `Serial` is the counter the send attaches, and stamping also makes this
    ///    process the token's `From` — and the event is dropped when no system tracer is
    ///    configured, exactly as `seq_trace:print/1`'s event is
    ///    (`erlang:system_flag(sequential_tracer, _)` is not implemented).
    ///
    /// # Errors
    ///
    /// Whatever the term writers report (a full heap); an undeliverable event is not an error
    /// (see [`RuntimeContext::emit_trace_event`]).
    pub(crate) fn trace_send_events(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        let sender = self.pid();
        let send_traced = self
            .runtime
            .trace()
            .get(sender)
            .is_some_and(|entry| entry.flags.0 & SEND != 0);
        if send_traced {
            self.emit_trace_event(sender, "send", &[message, to])?;
        }
        let send_token = self
            .runtime
            .seq_trace()
            .token(sender)
            .filter(|token| token.flags & crate::seq_trace::FLAG_SEND != 0);
        if let Some(token) = send_token {
            let from = sender.to_eterm();
            let stamped = self
                .runtime
                .seq_trace_mut()
                .advance_serial(sender, from)
                .unwrap_or(token);
            let tracer = self.runtime.seq_trace().tracer();
            if let Some(tracer) = tracer {
                let event = self.seq_trace_send_message(stamped, to, message)?;
                match self.deliver_message(tracer, from, event) {
                    // As for the ordinary event: no tracer process means the event is dropped,
                    // never a failed send.
                    Err(BeamError::TermNotFound) => {}
                    delivered => delivered?,
                }
            }
        }
        Ok(())
    }

    /// Emits the `{trace, Receiver, 'receive', Msg}` event a **delivered** message produces for
    /// the receiving process — contract A.8, item 1.
    ///
    /// The event is gated on the **receiver's** `receive` trace flag: the flag lives on the
    /// process the message arrives at, so a `receive`-traced receiver gets one event per message
    /// enqueued into its mailbox and a *sender* with the flag gets nothing.
    /// [`RuntimeContext::emit_trace_event`] does the rest (the tracer lookup, the `silent` flag,
    /// the tracer-is-the-tracee discard and the time-stamp flavour).
    ///
    /// The send seam ([`RuntimeContext::send_message`](crate::context::RuntimeContext::send_message))
    /// calls this once per delivered message, immediately after the `send` event, which is where
    /// the C emits it (`erl_proc_sig_queue.c:8626-8630`, `handle_message_enqueued_tracing`): the
    /// event fires on **enqueue**, not on matching, so a message a traced process never matches
    /// still produces one, and the event carries only the message (no sender). The event is
    /// emitted through [`RuntimeContext::deliver_message`] — the same bypass the `send` event
    /// uses — so delivering a trace event can never itself produce a `receive` event.
    ///
    /// # Errors
    ///
    /// Whatever the term writers report (a full heap); an undeliverable event is not an error
    /// (see [`RuntimeContext::emit_trace_event`]).
    pub(crate) fn trace_receive_events(&mut self, receiver: ProcessId, message: Eterm) -> Result<()> {
        let receive_traced = self
            .runtime
            .trace()
            .get(receiver)
            .is_some_and(|entry| entry.flags.0 & RECEIVE != 0);
        if receive_traced {
            self.emit_trace_event(receiver, "receive", &[message])?;
        }
        Ok(())
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

    /// The `pub(crate)` accessors the execution-trace seam (`crate::context::trace`) reads:
    /// each is exactly the private bit it names, and nothing else. The bits are internal, so
    /// only the atom set is observable — the accessors are pinned against the decoders.
    #[test]
    fn the_flag_accessors_select_exactly_their_own_bit() {
        let calls = flags_from_atoms(&["call"]).expect("call");
        assert!(calls.has_calls());
        assert!(!calls.has_return_to());
        assert!(!calls.has_arity());

        let return_to = flags_from_atoms(&["return_to"]).expect("return_to");
        assert!(!return_to.has_calls());
        assert!(return_to.has_return_to());

        let arity = flags_from_atoms(&["arity"]).expect("arity");
        assert!(arity.has_arity());
        assert!(!arity.has_calls());

        // `all` sets every base flag, so all three accessors answer `true`.
        let all = flags_from_atoms(&["all"]).expect("all");
        assert!(all.has_calls() && all.has_return_to() && all.has_arity());

        // A default (no trace flags) answers `false` for every accessor.
        let none = TraceFlags::default();
        assert!(!none.has_calls() && !none.has_return_to() && !none.has_arity());

        // The `trace_pattern` flags are a *different* atom set: none of the three accessors
        // above may be reached through them, and each pattern accessor names its own bit.
        let local = pattern_flags_from_atoms(&["local"]).expect("local");
        assert!(local.is_local());
        assert!(!local.is_global());
        assert!(!local.is_silent());

        let global = pattern_flags_from_atoms(&["global"]).expect("global");
        assert!(global.is_global());
        assert!(!global.is_local());
        assert!(!global.is_silent());

        let silent = pattern_flags_from_atoms(&["silent"]).expect("silent");
        assert!(silent.is_silent());
        assert!(!silent.is_local() && !silent.is_global());

        // An empty `FlagList` is zero flags: every accessor is `false`.
        let empty = pattern_flags_from_atoms(&[]).expect("[]");
        assert!(!empty.is_local() && !empty.is_global() && !empty.is_silent());
    }

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

    // The event time stamps (the pure helpers) ----------------------------------------------

    #[test]
    fn the_wall_clock_triple_is_the_os_timestamp_split() {
        // 1_234_567 seconds and 890_123 microseconds: two whole mega-seconds and a remainder.
        assert_eq!(wall_clock_triple(Duration::new(1_234_567, 890_123_000)), (1, 234_567, 890_123));
        // The epoch itself, and a sub-second duration.
        assert_eq!(wall_clock_triple(Duration::ZERO), (0, 0, 0));
        assert_eq!(wall_clock_triple(Duration::from_micros(999_999)), (0, 0, 999_999));
        // Exactly one mega-second has no remainder.
        assert_eq!(wall_clock_triple(Duration::from_secs(1_000_000)), (1, 0, 0));
    }

    #[test]
    fn the_monotonic_elapsed_time_is_nanoseconds() {
        assert_eq!(elapsed_nanos(Duration::ZERO), 0);
        assert_eq!(elapsed_nanos(Duration::from_nanos(7)), 7);
        assert_eq!(elapsed_nanos(Duration::from_secs(3)), 3_000_000_000);
        // A duration too large for an `i64` saturates at the C's `i64` counter maximum.
        assert_eq!(elapsed_nanos(Duration::from_secs(u64::MAX)), i64::MAX);
    }

    #[test]
    fn the_timestamp_flavour_precedence_and_tag() {
        let flavour =
            |names: &[&str]| TraceTimestamp::select(flags_from_atoms(names).expect("flags"));
        assert_eq!(flavour(&["send"]), None);
        assert_eq!(flavour(&["send", "timestamp"]), Some(TraceTimestamp::Wall));
        assert_eq!(flavour(&["send", "monotonic_timestamp"]), Some(TraceTimestamp::Monotonic));
        assert_eq!(
            flavour(&["send", "strict_monotonic_timestamp"]),
            Some(TraceTimestamp::StrictMonotonic)
        );
        // Precedence: `timestamp` > `strict_monotonic_timestamp` > `monotonic_timestamp`.
        assert_eq!(
            flavour(&["timestamp", "strict_monotonic_timestamp"]),
            Some(TraceTimestamp::Wall)
        );
        assert_eq!(
            flavour(&["strict_monotonic_timestamp", "monotonic_timestamp"]),
            Some(TraceTimestamp::StrictMonotonic)
        );
        assert_eq!(
            flavour(&["timestamp", "strict_monotonic_timestamp", "monotonic_timestamp"]),
            Some(TraceTimestamp::Wall)
        );
        // Whichever flavour asked for it, the event is a `trace_ts`.
        for names in [
            vec!["timestamp"],
            vec!["monotonic_timestamp"],
            vec!["strict_monotonic_timestamp"],
        ] {
            let selected = flavour(&names).expect("a flavour");
            assert_eq!(selected.tag(), "trace_ts", "{names:?}");
        }
    }

    #[test]
    fn the_monotonic_and_unique_counters_only_go_forwards() {
        let first = monotonic_nanos();
        let second = monotonic_nanos();
        assert!(first >= 0, "the monotonic base is the first read, got {first}");
        assert!(second >= first, "{second} >= {first}");

        let unique = next_unique_monotonic();
        assert_eq!(next_unique_monotonic(), unique + 1, "strictly increasing");
        assert!(next_unique_monotonic() > unique);
    }

    /// Every message in `pid`'s mailbox, in mailbox order, removed as it is read.
    ///
    /// `pid` runs against a caller-owned [`beam_process::ProcessTable`] (what
    /// [`RuntimeContext::for_process`] is built for), so the table is handed back on each read.
    fn drain(
        rt: &mut crate::Runtime,
        table: &mut beam_process::ProcessTable,
        pid: ProcessId,
    ) -> Vec<Eterm> {
        let mut ctx = RuntimeContext::for_process(rt, table, pid);
        ctx.receive_reset().expect("timeout");
        let mut messages = Vec::new();
        while let Some(term) = ctx.receive_next().expect("receive_next") {
            messages.push(term);
            ctx.receive_remove().expect("remove_message");
            assert!(messages.len() < 16, "the mailbox keeps draining: {messages:?}");
        }
        messages
    }

    /// A two-process runtime: `(sender, tracer)` live in a caller-owned table so the tracer can
    /// be a **different** process from the tracee (ERTS discards an event whose tracer is the
    /// tracee itself, so a self-traced process observes nothing).
    fn two_processes() -> (
        crate::Runtime,
        beam_process::ProcessTable,
        ProcessId,
        ProcessId,
    ) {
        let rt = crate::Runtime::with_defaults().expect("the default runtime");
        let mut atoms = beam_term::prelude::AtomTable::new();
        let mut table =
            beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(&mut atoms));
        let sender = table.spawn_default().expect("spawn the sender");
        let tracer = table.spawn_default().expect("spawn the tracer");
        (rt, table, sender, tracer)
    }

    #[test]
    fn a_send_traced_sender_emits_the_send_event_and_the_seq_trace_event() {
        let (mut rt, mut table, sender, tracer) = two_processes();
        let label = rt.intern("a-label");
        let message = rt.intern("payload");
        let send = rt.intern("send");
        let trace = rt.intern("trace");
        let seq_trace = rt.intern("seq_trace");
        // The sender is `[send]`-traced and reports to the **other** process, and its
        // sequential-trace token has the `send` flag with a system tracer configured (both
        // unreachable from outside the crate, which is why this case lives here).
        rt.trace_mut().set(
            sender,
            flags_from_atoms(&["send"]).expect("send"),
            Some(tracer),
        );
        rt.seq_trace_mut()
            .set_flag(sender, sender.to_eterm(), crate::seq_trace::FLAG_SEND, true);
        rt.seq_trace_mut().set_label(sender, sender.to_eterm(), label);
        rt.seq_trace_mut().set_tracer(Some(tracer));

        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, sender);
            ctx.send_message(sender.to_eterm(), message).expect("send");
        }

        // The **sender's** mailbox holds only the payload: the events go to the tracer.
        assert_eq!(
            drain(&mut rt, &mut table, sender),
            vec![message],
            "only the payload reaches the sender"
        );

        // The tracer's mailbox holds exactly the two events, in order: the ordinary
        // `{trace, …}` event, then the sequential-trace `{seq_trace, …}` event.
        let messages = drain(&mut rt, &mut table, tracer);
        assert_eq!(messages.len(), 2, "the two events: {messages:?}");

        // 1. `{trace, Sender, send, Msg, To}`.
        let event = messages[0];
        assert_eq!(terms::tuple_arity(event).expect("a tuple"), 5);
        assert_eq!(terms::tuple_element(event, 0).expect("the tag"), trace);
        assert_eq!(terms::tuple_element(event, 1).expect("the tracee"), sender.to_eterm());
        assert_eq!(terms::tuple_element(event, 2).expect("the event"), send);
        assert_eq!(terms::tuple_element(event, 3).expect("the message"), message);
        assert_eq!(
            terms::tuple_element(event, 4).expect("the destination"),
            sender.to_eterm()
        );

        // 2. `{seq_trace, Label, {send, {Previous, Current}, From, To, Msg}}`.
        let event = messages[1];
        assert_eq!(terms::tuple_arity(event).expect("a tuple"), 3);
        assert_eq!(terms::tuple_element(event, 0).expect("the tag"), seq_trace);
        assert_eq!(terms::tuple_element(event, 1).expect("the label"), label);
        let payload = terms::tuple_element(event, 2).expect("the payload");
        assert_eq!(terms::tuple_arity(payload).expect("a tuple"), 5);
        assert_eq!(terms::tuple_element(payload, 0).expect("the event"), send);
        let counters = terms::tuple_element(payload, 1).expect("the serial");
        // `Serial` is the documented `{Previous, Current}` pair; the send stamped it, so the
        // current counter is 1 (`seq_trace.erl:88-90`).
        assert_eq!(terms::tuple_element(counters, 0).expect("previous"), make_small(0));
        assert_eq!(terms::tuple_element(counters, 1).expect("current"), make_small(1));
        assert_eq!(terms::tuple_element(payload, 2).expect("from"), sender.to_eterm());
        assert_eq!(terms::tuple_element(payload, 3).expect("to"), sender.to_eterm());
        assert_eq!(terms::tuple_element(payload, 4).expect("the message"), message);
    }

    /// ERTS discards a trace event whose tracer **is** the tracee
    /// (`erts/emulator/nifs/common/erl_tracer_nif.c:143-146`, "Only generate trace for when
    /// tracer != tracee"), so a self-traced process observes no events of its own — the oracle
    /// answers `timeout` for the `receive` after a `erlang:trace(self(), true, [send])` self-send.
    #[test]
    fn a_self_traced_process_emits_no_event_to_itself() {
        let mut rt = crate::Runtime::with_defaults().expect("the default runtime");
        let pid = rt.self_pid();
        let message = rt.intern("payload");
        // The sender traces *itself*: tracer == tracee, so the `send` event is discarded.
        rt.trace_mut().set(
            pid,
            flags_from_atoms(&["send"]).expect("send"),
            Some(pid),
        );

        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
        ctx.send_message(pid.to_eterm(), message).expect("send");

        // The mailbox holds the payload and **nothing else** — no `{trace, …}` event.
        ctx.receive_reset().expect("timeout");
        let mut messages = Vec::new();
        while let Some(term) = ctx.receive_next().expect("receive_next") {
            messages.push(term);
            ctx.receive_remove().expect("remove_message");
            assert!(messages.len() < 8, "the mailbox keeps draining: {messages:?}");
        }
        assert_eq!(messages, vec![message], "the discard rule suppressed the event");
    }

    /// A **`['receive']`-traced** receiver gets `{trace, Receiver, 'receive', Msg}` when a
    /// message is delivered to it — and the flag lives on the receiver, so a `[send]`-traced
    /// *sender* does not produce it for an untraced receiver. Probe (OTP 29.1.1 / erts-17.1):
    /// tracing `Q` with `['receive']` and sending `Q` a message the process never matches still
    /// yields `{trace, Q, 'receive', never_matched}` (the event fires on enqueue).
    #[test]
    fn a_receive_traced_receiver_emits_the_receive_event() {
        let (mut rt, mut table, sender, tracer) = two_processes();
        let message = rt.intern("payload");
        let receive = rt.intern("receive");
        let trace = rt.intern("trace");
        // The **tracer** is traced with `['receive']` and reports to the sender (a third party,
        // distinct from the tracee): the tracer receives `{trace, tracer, 'receive', Msg}` in
        // its own mailbox when the *sender* delivers to it.
        rt.trace_mut().set(
            tracer,
            flags_from_atoms(&["receive"]).expect("receive"),
            Some(sender),
        );

        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, sender);
            ctx.send_message(tracer.to_eterm(), message).expect("send");
        }

        // The receive-traced **receiver's** mailbox holds only the payload: its event reports to
        // the tracer, not to itself.
        assert_eq!(
            drain(&mut rt, &mut table, tracer),
            vec![message],
            "only the payload reaches the receiver"
        );

        // The tracer's mailbox holds the `receive` event, about the receiver.
        let events = drain(&mut rt, &mut table, sender);
        assert_eq!(events.len(), 1, "the receive event: {events:?}");
        let event = events[0];
        assert_eq!(terms::tuple_arity(event).expect("a tuple"), 4);
        assert_eq!(terms::tuple_element(event, 0).expect("the tag"), trace);
        assert_eq!(
            terms::tuple_element(event, 1).expect("the tracee"),
            tracer.to_eterm(),
            "the event is about the *receiver*"
        );
        assert_eq!(terms::tuple_element(event, 2).expect("the event"), receive);
        assert_eq!(terms::tuple_element(event, 3).expect("the message"), message);
    }

    /// A `[send]`-traced sender delivering to an **untraced** receiver emits only the `send`
    /// event: the `receive` flag on the sender produces nothing for the receiver.
    #[test]
    fn the_receive_event_is_gated_on_the_receiver_not_the_sender() {
        let (mut rt, mut table, sender, receiver) = two_processes();
        let message = rt.intern("payload");
        let send = rt.intern("send");
        // The *sender* carries both flags and reports to the receiver; the receiver is untraced,
        // so it must contribute no `receive` event of its own.
        rt.trace_mut().set(
            sender,
            flags_from_atoms(&["send", "receive"]).expect("send+receive"),
            Some(receiver),
        );

        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, sender);
            ctx.send_message(receiver.to_eterm(), message).expect("send");
        }

        // The untraced receiver's mailbox holds the payload and the one `send` event for the
        // *sending* process, and nothing else; the sender's own mailbox is empty.
        let received = drain(&mut rt, &mut table, receiver);
        assert_eq!(received.len(), 2, "the payload and the send event: {received:?}");
        assert_eq!(received[0], message);
        assert_eq!(terms::tuple_element(received[1], 2).expect("the event"), send);
        assert_eq!(
            drain(&mut rt, &mut table, sender),
            Vec::new(),
            "a `receive` flag on the sender produces no event for the receiver"
        );
    }    // The trace-pattern model ---------------------------------------------------------------

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
