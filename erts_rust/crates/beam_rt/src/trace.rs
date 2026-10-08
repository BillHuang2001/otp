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
//!   `Pid | all | existing | existing_processes | processes`.
//!
//! # Deliberately out of scope (a later stage)
//!
//! * **Emitting `{trace, …}`/`{trace_ts, …}` event messages.** Nothing here is wired into the
//!   engine's send/receive/call paths, so setting a flag never produces an event. The flag state
//!   is real (and `trace_info` reports it), but no message is emitted.
//! * `trace_pattern` / `erts_internal:trace_pattern/*` (match-spec compilation) and `trace_info`'s
//!   `match_spec` is therefore always the empty list `[]`.
//! * The `new`/`new_processes`/`ports`/`existing_ports`/`new_ports` PidSpecs of
//!   `erts_internal:trace/3` — they are the honest typed capability gap (port/`new`-style trace
//!   targets are not modelled).
//! * Tracing **functions** (`trace_info({M,F,A}, …)`): a non-pid first argument is `badarg`, as
//!   the C is for an MFA this stage cannot resolve.
//!
//! Legacy C ground truth (read-only): `../../erts/emulator/beam/erl_bif_trace.c` (`trace_3`,
//! `trace_info_2`, `trace_delivered_1`) and `../../erts/emulator/beam/erl_trace.c`.

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

/// One traced process: its flags and the tracer it reports to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceEntry {
    /// The trace flags set on the process.
    pub flags: TraceFlags,
    /// The process the tracee reports to, if a tracer was recorded.
    pub tracer: Option<ProcessId>,
}

/// The node-global trace-flag state: one [`TraceEntry`] per traced process, plus the monotonic
/// delivery counter behind `erlang:trace_delivered/1`.
///
/// It is **pure** — no runtime borrow, no terms — so it is unit-testable on its own.
#[derive(Debug, Default)]
pub struct TraceState {
    entries: HashMap<ProcessId, TraceEntry>,
    clock: u64,
}

impl TraceState {
    /// An empty trace state.
    pub fn new() -> TraceState {
        TraceState::default()
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
    /// | `match_spec` | `{match_spec, []}` — match specs are not implemented yet |
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
}
