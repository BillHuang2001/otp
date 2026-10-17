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

//! Monitors and monitor references: [`Monitor`], [`MonitoredBy`], [`Monitors`],
//! [`MonitorRef`] and [`RefAllocator`].
//!
//! Monitors are one-directional and carry a reference: `A` monitoring `B` stores an
//! outgoing [`Monitor`] in `A` and an incoming [`MonitoredBy`] in `B`, and `B`'s
//! termination sends `A` a `{'DOWN', Ref, process, B, Reason}` message carrying that same
//! reference.
//!
//! # Process monitors vs port monitors
//!
//! [`MonitorKind`] names the two monitor types the oracle supports (`process` and `port`), and
//! [`MonitorKind::from_type_name`]/[`MonitorKind::type_name`] are the type-dispatch seam the
//! `erlang:monitor/2` BIF layer resolves its first argument through (`time_offset` and every
//! other atom are not a monitor type). A **port** monitor's target is a
//! [`PortId`](crate::pid::PortId) rather than a [`ProcessId`], so its outgoing half is a
//! [`PortWatch`] (stored on the watcher by
//! [`ProcessTable::monitor_port`](crate::ptab::ProcessTable::monitor_port)) and its incoming
//! half lives in the [`PortTable`](crate::port::PortTable).
//!
//! # Monitor references
//!
//! `beam_term` has no `make_ref` in Milestone-1, so a reference is represented as the
//! frozen 4-word ref thing `[make_header(3, REF_SUBTAG), node(0), serial, id]`, built here
//! through `beam_term`'s checked writer `write_internal_ref` (see the crate `CONTEXT.md`,
//! Design Decisions).
//! `beam_message::layout::object_words` copies such a thing verbatim and
//! `beam_term::compare` compares internal refs by payload words, so a `{'DOWN', …}` message
//! round-trips and matches; when `beam_term` gains a proper `make_ref`, only
//! [`MonitorRef::to_eterm`] changes.
//!
//! # User tags (`erlang:monitor/3` `{tag, Tag}`)
//!
//! The `erlang:monitor/3` `{tag, Tag}` option replaces the `'DOWN'` head atom of the
//! notification: `None` reproduces the ordinary `{'DOWN', Ref, Kind, Target, Reason}`, and
//! `Some(tag)` produces the oracle's `{Tag, Ref, Kind, Target, Reason}`.
//!
//! The tag is deliberately **not** a field of the records below: [`Monitor`],
//! [`MonitoredBy`] and [`PortWatch`] are built by struct literal from other crates (e.g.
//! `beam_rt`), so their shape is fixed and adding a field would break them. It is kept
//! instead in a **side table** on [`ProcessTable`](crate::ptab::ProcessTable), keyed by the
//! monitor reference — exactly like the `{Name, node}` descriptor of a named monitor (see
//! `ptab.rs`, Design Decisions). An entry is created only when a tag is supplied and is
//! removed the moment its monitor dies.
//!
//! A tag must be **heap-independent**: an immediate (an atom, small integer, local
//! pid/port, `nil`) or a literal-area term. The side table holds only plain values — the
//! crate's collector never scans it — so a term resident in a *per-process* heap could not
//! be relocated and would dangle after a collection; a literal-area term, by contrast, is
//! shared and never moves. A tag that is not an immediate is therefore rejected by the
//! `ptab.rs` entry points with a typed `unsupported`, so a caller never gets a wrong result.
//!
//! # Reference aliases (`erlang:alias/1`, `erlang:unalias/1`, `erlang:monitor/3` `{alias, Mode}`)
//!
//! An **alias** is a reference a process may be **sent to**: `Alias ! Msg` delivers `Msg` to the
//! process that created the alias, and the alias **expires** according to its [`UnaliasMode`].
//! `erlang:alias/1` mints a standalone alias; `erlang:monitor/3`'s `{alias, Mode}` option makes a
//! monitor's own reference double as one (the C spells both with `erts_make_pid_ref`, the
//! pid-carrying reference).
//!
//! The mode is what makes an alias more than a ref, and its rule is applied at exactly two
//! points (see `ptab.rs`):
//!
//! * where the monitor's **DOWN** is produced, and
//! * where the monitor is removed by an explicit `erlang:demonitor/1`.
//!
//! `explicit_unalias` survives **both** (it lives on as a standalone alias until an explicit
//! `erlang:unalias/1`), `demonitor` dies at the DOWN, and `reply_demonitor` dies at the DOWN *and*
//! the moment a message is delivered through it (the reply).
//!
//! The creator — the process a message sent to the alias is delivered to — is not part of the
//! reference here (this crate's [`MonitorRef`] is a bare id; the C packs the creator pid into the
//! reference), so it is carried beside the mode in the [`AliasEntry`] side table on
//! [`ProcessTable`](crate::ptab::ProcessTable), exactly like the `{tag, Tag}` user tag above.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_monitor_link.c`,
//! `erts/emulator/beam/bif.c` (`erts_monitor_opts`, `monitor`, `demonitor`, `alias_1`,
//! `unalias_1`) and `erts/emulator/beam/erl_proc_sig_queue.c` (the alias-message and DOWN paths).

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    boxed_val, header_arity, header_subtag, internal_ref_id, is_boxed_term, is_tuple, make_atom,
    signed_value, try_make_small, tuple_element, tuple_size, write_internal_ref, Atom, Eterm,
    TermError, INTERNAL_REF_PAYLOAD_WORDS, INTERNAL_REF_WORDS, REF_SUBTAG,
};

use crate::error;
use crate::pid::{PortId, ProcessId};

/// Whether `id` is encodable as a reference id at all — the **checked** bound of
/// [`MonitorRef::to_eterm`], which is where the whole identity of a reference lives.
///
/// # The bound
///
/// A reference is an **immediate** in the frozen term layout, and its entire identity rides
/// the one small-integer word of `beam_term`'s 4-word ref thing
/// (`beam_term::boxed::INTERNAL_REF_WORDS`: `[header, node, serial, id]`, with `node`/`serial`
/// pinned to zero and the wire codec rebuilding them as zero), so the id space is exactly the
/// small-integer range: `MIN_SMALL..=MAX_SMALL` (`beam_term::immediate`, derived from
/// `TAG_IMMED1_SIZE`). An id is a `u64`, so the encodable half is `0..=MAX_SMALL`.
///
/// # Why it is *checked* rather than asserted
///
/// `make_small` is an unchecked `const fn` that only `debug_assert!`s its range (it mirrors
/// the C macro and sits on engine hot paths), so encoding an out-of-range id would **panic**
/// in a debug build and, in release, silently produce a *wrong term*: `1 << 59` shifted into
/// the immediate payload sets the sign bit, so the word decodes as a **negative** small and
/// distinct ids collide. Neither is acceptable for a value a peer can address a message to.
/// Widening the slot is not an option either (a bignum id needs a variable-size thing, which
/// breaks the frozen block — see `beam_term::boxed::INTERNAL_REF_WORDS`), so an id past the
/// bound is refused with the crate's typed resource-limit error
/// ([`error::system_limit`] ⇒ `BeamError::SystemLimit`), exactly as pid/port number-space
/// exhaustion is.
///
/// # Is it reachable?
///
/// Only in principle: [`RefAllocator`] hands out one id per `erlang:monitor/2,3` /
/// `erlang:alias/1`, monotonically from `0`, so a node would need `MAX_SMALL + 1` monitors
/// before it reached the bound. It is therefore not a practical limit — but it *is* the
/// allocator's real limit, and a refusal is the only honest answer once it is passed: an
/// unchecked wrap would hand out two **equal** references, so a stale `{'DOWN', …}` could be
/// mistaken for a live monitor's (the invariant [`RefAllocator`] exists to keep).
fn ref_id_fits(id: u64) -> Result<()> {
    // `MIN_SMALL` needs no test: a `u64` id is never negative, and `i64::try_from` rejects the
    // whole upper half of the `u64` space.
    let signed = i64::try_from(id).map_err(|_| error::system_limit())?;
    match try_make_small(signed) {
        Ok(_) => Ok(()),
        Err(_) => Err(error::system_limit()),
    }
}

/// A monitor reference: the identity a `{'DOWN', …}` message carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef {
    id: u64,
}

impl MonitorRef {
    /// The numeric reference id.
    pub fn id(self) -> u64 {
        self.id
    }

    /// Builds the on-heap ref thing for this reference (see the module docs).
    ///
    /// The thing is `[make_header(3, REF_SUBTAG), node, serial, id]` — Milestone-1 has a
    /// single monotonic id space, so `node` and `serial` are zero and the whole id lives in
    /// the last word (a `small`). The block is written by `beam_term`'s checked
    /// `write_internal_ref`, so this producer and the crate's other reference writers cannot
    /// drift out of the frozen layout.
    ///
    /// # Errors
    ///
    /// * `BeamError::Alloc` when `heap` cannot fit the ref thing.
    /// * `BeamError::SystemLimit` when this reference's id does not fit the block's single
    ///   small-integer id word — the bound [`ref_id_fits`] documents and *checks* (never a
    ///   silently wrong term, never a panic, never a layout-breaking bignum).
    pub fn to_eterm(self, heap: &mut Heap) -> Result<Eterm> {
        // Refuse an unencodable id **before** reserving heap words, so the refusal allocates
        // nothing at all.
        ref_id_fits(self.id)?;
        // SAFETY: `alloc_words` returns `INTERNAL_REF_WORDS` exclusively-owned, writable words.
        let p = unsafe { heap.alloc_words(INTERNAL_REF_WORDS)? };
        // SAFETY: `p` addresses exactly the words just handed out and nothing else writes them;
        // `write_internal_ref` initialises all `INTERNAL_REF_WORDS` of them before handing the
        // pointer back inside the returned root.
        match unsafe { write_internal_ref(p, self.id) } {
            Ok(term) => Ok(term),
            // Unreachable after `ref_id_fits` — `write_internal_ref`'s only error is the very
            // range check just done — but mapped, never `unwrap`ped, so the pair cannot panic.
            Err(TermError::SmallIntOutOfRange) => Err(error::system_limit()),
            // No other variant exists today; propagating it keeps this honest if one appears.
            Err(other) => Err(other.into()),
        }
    }

    /// Decodes an on-heap internal reference thing — the inverse of [`MonitorRef::to_eterm`].
    ///
    /// This is the **destination-to-key** seam of the send path: a message addressed to a
    /// reference (an `erlang:alias/1` alias, or a `erlang:monitor/3` `{alias, Mode}` monitor's
    /// own reference) carries the reference **term**, which must be turned back into the
    /// [`MonitorRef`] the table's alias/demonitor lookup is keyed by.
    ///
    /// # Errors
    ///
    /// Returns [`BeamError::BadArg`] for anything that is not an internal reference: a non-term
    /// word, a boxed term of another kind, a C-layout reference block, or a block whose id word
    /// is not a non-negative small integer.
    ///
    /// # Robustness
    ///
    /// The read is delegated to `beam_term`'s sanctioned M1 reference accessor
    /// (`internal_ref_id`), which proves the boxed-ness, the `REF_SUBTAG` header **and** the
    /// payload arity before it touches the id word, so no word is ever read before its existence
    /// is established (a C-layout three-word block is refused rather than over-read). Its
    /// `TermError::BadRef` maps to `BadArg` — the oracle's answer for a non-reference argument.
    pub fn from_eterm(term: Eterm) -> Result<MonitorRef> {
        // SAFETY: `internal_ref_id` validates every read itself — it rejects a word that is not a
        // boxed term, a boxed term whose header does not carry `REF_SUBTAG`, a C-layout ref block
        // (arity 2) and a block whose id word is not a non-negative small integer.
        let id = unsafe { internal_ref_id(term) }?;
        Ok(MonitorRef { id })
    }
}

/// When a reference **alias** expires — the `erlang:alias/1` and `erlang:monitor/3`
/// `{alias, Mode}` mode.
///
/// The atom-to-mode parse (`explicit_unalias` / `demonitor` / `reply_demonitor`) is the BIF
/// layer's job (`beam_rt`); this enum is the typed decision the table acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaliasMode {
    /// `explicit_unalias` (the C `ERTS_ML_STATE_ALIAS_UNALIAS`, and `erlang:alias/0`'s default):
    /// the alias is removed **only** by an explicit `erlang:unalias/1`. The monitored target's
    /// DOWN does not remove it, and neither does an explicit `erlang:demonitor/1` — in both cases
    /// it lives on as a **standalone** alias (the C replaces the monitor entry with an
    /// `ERTS_MON_TYPE_ALIAS` one).
    ExplicitUnalias,
    /// `demonitor` (the C `ERTS_ML_STATE_ALIAS_DEMONITOR`): the alias is removed when the
    /// monitor's DOWN is delivered, or when an explicit `erlang:demonitor/1` removes the monitor.
    /// A message delivered through it does **not** remove it.
    Demonitor,
    /// `reply_demonitor` (the C `ERTS_ML_STATE_ALIAS_ONCE`, `erlang:alias([reply])`): the alias is
    /// removed when a message is delivered through it (the reply) **and** when the monitor's DOWN
    /// is delivered (or an explicit `erlang:demonitor/1` removes the monitor).
    ReplyDemonitor,
}

/// One live alias: **who** a message sent to the reference is delivered to, and **when** the alias
/// expires.
///
/// The C packs the creator's pid into the reference itself (`erts_make_pid_ref`) and keeps the
/// mode in the monitor entry's flags; this crate's [`MonitorRef`] is a bare id, so both are held
/// here instead, in the [`ProcessTable`](crate::ptab::ProcessTable) alias side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AliasEntry {
    /// The process that created the alias (`erlang:alias/1`'s caller, or the `watcher` of a
    /// `erlang:monitor/3` `{alias, Mode}` monitor): the process a message sent to the reference is
    /// **delivered to**.
    pub owner: ProcessId,
    /// The rule that expires this alias.
    pub mode: UnaliasMode,
}

/// Whether `payload` is the `{'DOWN', Reference, _, _, _}` message for `reference`.
///
/// A `demonitor(Ref, [flush])` helper: it recognises the monitor-DOWN message a
/// [`MonitorRef`] produced, so the caller can drop it out of a mailbox.
///
/// # Encoding
///
/// The id is the ref thing's **last payload word**
/// (`INTERNAL_REF_PAYLOAD_WORDS`): [`MonitorRef::to_eterm`] builds the frozen
/// `[make_header(3, REF_SUBTAG), node, serial, id]`, so that word holds
/// `make_small(reference.id())` (`monitor_ref_to_eterm_builds_the_frozen_thing` pins exactly
/// that layout). The identity test is therefore "word
/// [`INTERNAL_REF_PAYLOAD_WORDS`] decodes to `reference.id()`", spelled **totally and
/// sign-safely**: the word is read as a sign-extended small payload and then converted with
/// `u64::try_from`, so a **negative** id word (a corrupt or foreign block) can never compare
/// equal to a `u64` reference id — the old `word as i64 == id as i64` form wrapped both sides
/// and could match a huge id against a negative word.
///
/// # Robustness
///
/// Every read is guarded: a non-tuple payload, a payload that is not a 5-tuple, a head element
/// that is not `down_atom`, and an element 1 that is not a boxed ref thing are all `false`
/// without reading a word the guards have not first proven to exist. The ref block must carry
/// **both** `REF_SUBTAG` **and** arity [`INTERNAL_REF_PAYLOAD_WORDS`], because the subtag alone
/// is also worn by the C-layout `ErtsORefThing` (arity 2, three words — no id slot), so the
/// arity is what proves the id word exists rather than over-reading a shorter block. It never
/// panics.
pub fn payload_is_down_for(payload: Eterm, reference: MonitorRef, down_atom: Atom) -> bool {
    // SAFETY: the guards form a chain. `is_tuple` first rejects everything that is not a live
    // boxed term whose header word is a valid arity value, so `tuple_size`/`tuple_element` only
    // ever read a live tuple; the arity test bounds every element read to `0..5`; and element 1
    // is only dereferenced after `is_boxed_term` proved it a dereferenceable boxed term, and
    // `header_subtag` + `header_arity` proved its header carries `REF_SUBTAG` with
    // `INTERNAL_REF_PAYLOAD_WORDS` payload words, so word `INTERNAL_REF_PAYLOAD_WORDS` (the id)
    // is in bounds. No path reads a word it has not first proven to exist.
    unsafe {
        if !is_tuple(payload) || tuple_size(payload) != 5 {
            return false;
        }
        if tuple_element(payload, 0) != make_atom(down_atom) {
            return false;
        }
        let reference_term = tuple_element(payload, 1);
        if !is_boxed_term(reference_term) {
            return false;
        }
        let words = boxed_val(reference_term);
        if header_subtag(*words) != REF_SUBTAG as u32
            || header_arity(*words) != INTERNAL_REF_PAYLOAD_WORDS
        {
            return false;
        }
        u64::try_from(signed_value(*words.add(INTERNAL_REF_PAYLOAD_WORDS))).ok()
            == Some(reference.id())
    }
}

/// What a monitor targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MonitorKind {
    /// A process monitor (`erlang:monitor(process, Target)`).
    Process,
    /// A port monitor (`erlang:monitor(port, Target)`).
    Port,
}

impl MonitorKind {
    /// The `erlang:monitor/2` **type name** this kind is spelled with: `"process"` or
    /// `"port"`.
    pub fn type_name(self) -> &'static str {
        match self {
            MonitorKind::Process => "process",
            MonitorKind::Port => "port",
        }
    }

    /// Maps an `erlang:monitor/2` **type name** to a monitor kind, or `None` when the name
    /// is not a supported monitor type.
    ///
    /// This is the type-dispatch seam the BIF layer resolves `erlang:monitor/2`'s first
    /// argument through; a `None` is the oracle's `badarg`. `time_offset` — the monitor type
    /// OTP removed in OTP 26 — and every other atom are `None`, so this crate has **no**
    /// `time_offset` (or any other unsupported-type) path capable of returning a wrong
    /// result: there is no such variant to select.
    ///
    /// Note that only the *type name* is resolved here; the target still has to match the
    /// kind (`erlang:monitor(port, NotAPort)` is `badarg` — see
    /// [`ProcessTable::monitor_port_term`](crate::ptab::ProcessTable::monitor_port_term)).
    pub fn from_type_name(name: &str) -> Option<MonitorKind> {
        match name {
            "process" => Some(MonitorKind::Process),
            "port" => Some(MonitorKind::Port),
            _ => None,
        }
    }
}

/// An outgoing **port** monitor: [`Process`](crate::process::Process) `watcher` watches `port`.
///
/// A port monitor is one-directional exactly like a process monitor, but its target is a
/// [`PortId`] rather than a [`ProcessId`]:
/// [`ProcessTable::monitor_port`](crate::ptab::ProcessTable::monitor_port) records this
/// outgoing half on the watcher and the incoming
/// [`PortMonitor`](crate::port::PortMonitor) on the port, and
/// [`ProcessTable::port_closed`](crate::ptab::ProcessTable::port_closed) resolves the pair with
/// `{'DOWN', Ref, port, Port, Reason}` (or `{Tag, Ref, port, Port, Reason}` when the monitor
/// carried an `erlang:monitor/3` user tag).
///
/// # Not a GC root
///
/// `reference` and `port` are plain values (a `u64`-backed handle), so a port-watch set adds
/// no root to the collector:
/// [`Process::collect`](crate::process::Process::collect) gathers only the
/// registers/stack, the mailbox payloads, the dictionary, the saved args and the
/// pending-signal queue. (A port monitor therefore also never affects `beam_message`'s copy
/// sizing, which only walks a crossed term's own object graph.) A monitor's
/// `erlang:monitor/3` user tag lives in `ProcessTable`'s side table, not here (see the module
/// docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortWatch {
    /// The reference the `{'DOWN', …}` message will carry.
    pub reference: MonitorRef,
    /// The watched port.
    pub port: PortId,
}

/// An outgoing monitor: this process watches `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    /// The reference the `{'DOWN', …}` message will carry.
    pub reference: MonitorRef,
    /// The watched process.
    pub target: ProcessId,
    /// Whether a process or a port is watched.
    pub kind: MonitorKind,
}

/// An incoming monitor: `watcher` watches this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoredBy {
    /// The reference the `{'DOWN', …}` message must carry.
    pub reference: MonitorRef,
    /// The watching process.
    pub watcher: ProcessId,
    /// Whether a process or a port is watched.
    pub kind: MonitorKind,
}

/// The outgoing-monitor set of one process.
pub struct Monitors {
    entries: Vec<Monitor>,
}

impl Monitors {
    /// An empty monitor set.
    pub fn new() -> Monitors {
        Monitors {
            entries: Vec::new(),
        }
    }

    /// The number of outgoing monitors.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the process has no outgoing monitors.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Adds an outgoing monitor.
    pub fn push(&mut self, monitor: Monitor) {
        self.entries.push(monitor);
    }

    /// Removes and returns the monitor with `reference`, if present.
    pub fn remove(&mut self, reference: MonitorRef) -> Option<Monitor> {
        let index = self.index_of(reference)?;
        Some(self.entries.remove(index))
    }

    /// Iterates the outgoing monitors.
    pub fn iter(&self) -> impl Iterator<Item = &Monitor> + '_ {
        self.entries.iter()
    }

    /// Borrows the monitor with `reference`, if present.
    pub fn find(&self, reference: MonitorRef) -> Option<&Monitor> {
        self.entries
            .iter()
            .find(|monitor| monitor.reference == reference)
    }

    /// Takes every outgoing monitor, leaving the set empty.
    pub fn drain(&mut self) -> Vec<Monitor> {
        core::mem::take(&mut self.entries)
    }

    /// The position of the monitor with `reference`, if present.
    fn index_of(&self, reference: MonitorRef) -> Option<usize> {
        self.entries
            .iter()
            .position(|monitor| monitor.reference == reference)
    }
}

/// The monotonic reference allocator, owned by the
/// [`ProcessTable`](crate::ptab::ProcessTable).
///
/// A reference id is never reused, so a stale `{'DOWN', …}` can never be confused with a
/// live monitor.
///
/// # The real limit is the encodable one, and it is checked at the encoder
///
/// The counter is a `u64`, but an *encodable* reference id is only `0..=MAX_SMALL`
/// (`2^59 - 1`): the whole identity of a reference rides the one small-integer word of
/// `beam_term`'s frozen 4-word ref thing (see [`ref_id_fits`]). [`RefAllocator::next`] is
/// therefore deliberately **total and infallible** — sibling crates call it as
/// `RefAllocator::new().next()` — and [`MonitorRef::to_eterm`] is the single **checked** gate:
/// an id past the bound is refused there with `BeamError::SystemLimit`, so no id is ever
/// truncated, substituted or wrapped into a *different* reference. Exhaustion is thus a typed
/// resource limit (like pid/port number-space exhaustion), not an invariant taken on trust.
///
/// The `wrapping_add` in [`RefAllocator::next`] is consequently **not** a claim that the wrap
/// point is reachable: it only keeps the counter itself total rather than panicking. The
/// allocator stops being able to *produce a usable reference* at `MAX_SMALL + 1` — about
/// `5.8e17` monitors, far below the `2^64` wrap — so an id can never be handed out, refused,
/// and then handed out again under the wrap.
pub struct RefAllocator {
    next: u64,
}

impl RefAllocator {
    /// An allocator that will hand out its first reference on the first call.
    pub fn new() -> RefAllocator {
        RefAllocator { next: 0 }
    }

    /// Hands out the next never-reused reference.
    ///
    /// This never fails; what can fail is **encoding** the returned id as a term
    /// ([`MonitorRef::to_eterm`] refuses an id past `MAX_SMALL` with `BeamError::SystemLimit`).
    /// `wrapping_add` only keeps the counter total: the wrap point lies far beyond the
    /// encodable bound, so no observable reference is ever reused (see the type docs).
    pub fn next(&mut self) -> MonitorRef {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        MonitorRef { id }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for `RefAllocator` monotonicity, the `Monitors` set operations, the
    //! frozen shape of the on-heap ref thing [`MonitorRef::to_eterm`] builds and its
    //! checked id bound.

    use beam_common::BeamError;
    use beam_term::prelude::{
        boxed_val, header_subtag, is_boxed, make_boxed, make_header, make_small, signed_value,
        MAX_SMALL, MIN_SMALL, REF_SUBTAG, SMALL_BITS,
    };

    use super::*;

    fn pid(number: u64) -> ProcessId {
        ProcessId::new(number, 0)
    }

    #[test]
    fn ref_allocator_hands_out_distinct_monotonic_ids() {
        let mut refs = RefAllocator::new();
        let a = refs.next();
        let b = refs.next();
        let c = refs.next();
        assert!(a.id() < b.id());
        assert!(b.id() < c.id());
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn monitor_kind_type_names_round_trip_and_reject_time_offset() {
        // Pins objective item 6: `erlang:monitor/2`'s type dispatch. Only `process` and
        // `port` are monitor types; `time_offset` (removed in OTP 26) and anything else is
        // `None`, which the BIF layer renders as the oracle's `badarg`.
        assert_eq!(MonitorKind::Process.type_name(), "process");
        assert_eq!(MonitorKind::Port.type_name(), "port");
        assert_eq!(
            MonitorKind::from_type_name("process"),
            Some(MonitorKind::Process)
        );
        assert_eq!(MonitorKind::from_type_name("port"), Some(MonitorKind::Port));
        assert_eq!(MonitorKind::from_type_name("time_offset"), None);
        assert_eq!(MonitorKind::from_type_name("no_such_type"), None);
        assert_eq!(MonitorKind::from_type_name(""), None);
        // Case matters: the oracle's atom is the exact lowercase `time_offset`/`process`.
        assert_eq!(MonitorKind::from_type_name("Process"), None);
        // Every kind round-trips through its own name.
        for kind in [MonitorKind::Process, MonitorKind::Port] {
            assert_eq!(MonitorKind::from_type_name(kind.type_name()), Some(kind));
        }
    }

    #[test]
    fn monitors_push_find_remove_and_drain() {
        let mut refs = RefAllocator::new();
        let r1 = refs.next();
        let r2 = refs.next();

        let mut monitors = Monitors::new();
        assert!(monitors.is_empty());
        assert_eq!(monitors.len(), 0);
        assert!(monitors.find(r1).is_none());

        monitors.push(Monitor {
            reference: r1,
            target: pid(1),
            kind: MonitorKind::Process,
        });
        monitors.push(Monitor {
            reference: r2,
            target: pid(2),
            kind: MonitorKind::Port,
        });
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors.find(r1).map(|m| m.target), Some(pid(1)));
        assert_eq!(monitors.find(r2).map(|m| m.kind), Some(MonitorKind::Port));
        assert_eq!(
            monitors.iter().map(|m| m.reference).collect::<Vec<_>>(),
            vec![r1, r2]
        );

        let removed = monitors.remove(r1).expect("r1 is present");
        assert_eq!(removed.target, pid(1));
        assert!(monitors.remove(r1).is_none());
        assert_eq!(monitors.len(), 1);

        let all = monitors.drain();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].reference, r2);
        assert!(monitors.is_empty());
    }

    #[test]
    fn monitor_ref_to_eterm_builds_the_frozen_thing() {
        let mut refs = RefAllocator::new();
        let reference = refs.next();
        // Skip one id so the encoded id is not simply zero.
        let _ = refs.next();
        let second = refs.next();

        let mut heap = Heap::new(8).expect("test heap");
        let term = reference.to_eterm(&mut heap).expect("the ref thing fits");
        assert!(is_boxed(term));
        assert_eq!(heap.used(), 4, "a ref thing is four words");

        // SAFETY: `term` is a live, four-word heap object; words 0..4 are in bounds and
        // were all written by `to_eterm`.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(header_subtag(*p), REF_SUBTAG as u32);
            assert_eq!(signed_value(*p.add(1)), 0, "node is local (0)");
            assert_eq!(signed_value(*p.add(2)), 0, "one id space in M1");
            assert_eq!(signed_value(*p.add(3)), reference.id() as i64);
            // Two distinct references encode to distinct last words.
            let other = second.to_eterm(&mut heap).expect("the ref thing fits");
            assert_ne!(*p.add(3), *boxed_val(other).add(3));
        }
    }

    #[test]
    fn monitor_ref_from_eterm_round_trips_and_rejects_every_non_ref() {
        use beam_term::prelude::AtomTable;

        let mut refs = RefAllocator::new();
        let reference = refs.next();
        let _ = refs.next();
        let second = refs.next();

        let mut heap = Heap::new(16).expect("test heap");
        let term = reference.to_eterm(&mut heap).expect("the ref thing fits");
        assert_eq!(
            MonitorRef::from_eterm(term).expect("an internal ref thing decodes"),
            reference
        );
        // A distinct reference decodes to its own id (the id lives in the last word).
        let other = second.to_eterm(&mut heap).expect("the ref thing fits");
        assert_eq!(MonitorRef::from_eterm(other).expect("decodes"), second);
        assert_ne!(
            MonitorRef::from_eterm(term).expect("decodes"),
            MonitorRef::from_eterm(other).expect("decodes")
        );

        // Non-references: a small integer, a pid immediate, a tuple, a bare header word (the ABI's
        // `THE_NON_VALUE`) — every one is `badarg`, never a wrong id.
        for bad in [
            make_small(7),
            pid(3).to_eterm(),
            make_atom(AtomTable::new().intern("not_a_ref")),
            make_header(3, REF_SUBTAG),
        ] {
            assert!(
                matches!(MonitorRef::from_eterm(bad), Err(BeamError::BadArg)),
                "a non-reference must be badarg: {bad:?}"
            );
        }
        let tuple = heap.tuple(&[make_small(1), make_small(2)]).expect("fits");
        assert!(matches!(
            MonitorRef::from_eterm(tuple),
            Err(BeamError::BadArg)
        ));

        // A C-layout reference block (header arity 2, three words) is refused rather than
        // over-read (its id slot does not exist).
        let c_layout = {
            let p = unsafe { heap.alloc_words(3) }.expect("fits");
            // SAFETY: the three words are exclusively owned and written before escaping.
            unsafe {
                *p = make_header(2, REF_SUBTAG);
                *p.add(1) = make_small(0);
                *p.add(2) = make_small(0);
            }
            make_boxed(p as *const Eterm)
        };
        assert!(matches!(
            MonitorRef::from_eterm(c_layout),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn to_eterm_reports_exhaustion_rather_than_panicking() {
        let mut heap = Heap::new(3).expect("a three-word heap");
        let reference = RefAllocator::new().next();
        assert!(matches!(
            reference.to_eterm(&mut heap),
            Err(BeamError::Alloc(_))
        ));
    }

    #[test]
    fn to_eterm_checks_the_ref_id_against_the_small_integer_bound() {
        // The whole identity of a ref thing rides its single id word, so the encodable id space
        // is exactly the small-integer one — `MIN_SMALL..=MAX_SMALL`, which `beam_term` derives
        // from the immediate tag width (`SMALL_BITS = 64 - TAG_IMMED1_SIZE`) and this test must
        // not restate as a literal.
        assert_eq!(MAX_SMALL, (1i64 << (SMALL_BITS - 1)) - 1);
        assert_eq!(MIN_SMALL, -MAX_SMALL - 1);
        // The boundary itself still encodes, byte-exactly as before, and round-trips.
        let at_bound = MonitorRef {
            id: MAX_SMALL as u64,
        };
        let mut heap = Heap::new(8).expect("test heap");
        let term = at_bound
            .to_eterm(&mut heap)
            .expect("the largest encodable id encodes");
        assert_eq!(heap.used(), 4, "a ref thing is four words");
        // SAFETY: `term` is a live, four-word heap object; words 0..4 are in bounds and were all
        // written by `to_eterm`.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(header_subtag(*p), REF_SUBTAG as u32);
            assert_eq!(signed_value(*p.add(1)), 0, "node is local (0)");
            assert_eq!(signed_value(*p.add(2)), 0, "one id space in M1");
            assert_eq!(
                signed_value(*p.add(INTERNAL_REF_PAYLOAD_WORDS)),
                MAX_SMALL,
                "the boundary id is the last word, still a positive small"
            );
        }
        assert_eq!(
            MonitorRef::from_eterm(term).expect("the boundary id decodes"),
            at_bound
        );

        // Everything past the bound is the typed resource limit: no panic (a debug-only
        // `debug_assert!` would fire in `make_small`), no silently wrong — and colliding —
        // negative small, no layout-breaking bignum, and no heap words reserved.
        for id in [
            MAX_SMALL as u64 + 1, // the very next id: sign-extended to a *negative* small
            ((MAX_SMALL as u64) << 1), // `2^60 - 2`
            i64::MAX as u64,      // still an `i64`, but far past the small range
            u64::MAX,             // not even an `i64`
        ] {
            let mut heap = Heap::new(8).expect("test heap");
            let refused = MonitorRef { id };
            assert!(
                matches!(refused.to_eterm(&mut heap), Err(BeamError::SystemLimit)),
                "id {id} must be refused with system_limit"
            );
            assert_eq!(heap.used(), 0, "a refused id reserves no heap words");
        }

        // Driving the *allocator* to the boundary: the last encodable draw works, the next one is
        // refused at the encoder (the allocator's own `next` stays total, by contract).
        let mut refs = RefAllocator::new();
        refs.next = MAX_SMALL as u64; // reachable only from an in-module test
        let last = refs.next();
        assert_eq!(last.id(), MAX_SMALL as u64);
        let mut heap = Heap::new(8).expect("test heap");
        assert!(last.to_eterm(&mut heap).is_ok());
        let overflowed = refs.next();
        assert_eq!(overflowed.id(), MAX_SMALL as u64 + 1);
        let mut heap = Heap::new(8).expect("test heap");
        assert!(matches!(
            overflowed.to_eterm(&mut heap),
            Err(BeamError::SystemLimit)
        ));

        // The ordinary draw path is unaffected: every id the allocator actually reaches early on
        // encodes and round-trips.
        let mut refs = RefAllocator::new();
        for _ in 0..64 {
            let reference = refs.next();
            let mut heap = Heap::new(8).expect("test heap");
            let term = reference
                .to_eterm(&mut heap)
                .expect("an ordinary id encodes");
            assert_eq!(
                MonitorRef::from_eterm(term).expect("an ordinary id decodes"),
                reference
            );
        }
    }

    #[test]
    fn payload_is_down_for_matches_only_its_own_reference_and_shape() {
        use beam_term::prelude::{make_atom, AtomTable};

        let mut table = AtomTable::new();
        let down = table.intern("DOWN");
        let exit = table.intern("EXIT");
        let process = table.intern("process");

        let mut refs = RefAllocator::new();
        let reference = refs.next();
        let other = refs.next();

        let mut heap = Heap::new(64).expect("test heap");
        let mut ref_heap = Heap::new(32).expect("test heap");
        let reference_term = reference.to_eterm(&mut ref_heap).expect("the ref fits");

        // The exact shape the signal builders produce.
        let message = heap
            .tuple(&[
                make_atom(down),
                reference_term,
                make_atom(process),
                pid(1).to_eterm(),
                make_small(0),
            ])
            .expect("the test arena holds the tuple");
        assert!(payload_is_down_for(message, reference, down));
        // A different reference is not this message's DOWN ...
        assert!(!payload_is_down_for(message, other, down));
        // ... and a different head atom is not a DOWN at all.
        assert!(!payload_is_down_for(message, reference, exit));

        // A 4-tuple with the right head is not a 5-tuple DOWN.
        let short = heap
            .tuple(&[
                make_atom(down),
                reference_term,
                make_atom(process),
                pid(1).to_eterm(),
            ])
            .expect("fits");
        assert!(!payload_is_down_for(short, reference, down));

        // Element 1 that is not a boxed ref thing (here: a pid immediate).
        let not_ref = heap
            .tuple(&[
                make_atom(down),
                pid(2).to_eterm(),
                make_atom(process),
                pid(1).to_eterm(),
                make_small(0),
            ])
            .expect("fits");
        assert!(!payload_is_down_for(not_ref, reference, down));

        // A **C-layout** reference block (header arity 2, three words — no id slot) wears the
        // same `REF_SUBTAG`, so it must be refused on arity rather than over-read for its id.
        let c_layout_ref = {
            let p = unsafe { ref_heap.alloc_words(3) }.expect("fits");
            // SAFETY: the three words are exclusively owned and written before escaping.
            unsafe {
                *p = make_header(2, REF_SUBTAG);
                *p.add(1) = make_small(0);
                *p.add(2) = make_small(0);
            }
            make_boxed(p as *const Eterm)
        };
        let c_layout_down = heap
            .tuple(&[
                make_atom(down),
                c_layout_ref,
                make_atom(process),
                pid(1).to_eterm(),
                make_small(0),
            ])
            .expect("fits");
        assert!(!payload_is_down_for(c_layout_down, reference, down));

        // A ref block whose id word is *negative* is never an id: `u64::try_from` refuses it, so
        // it cannot match even a reference whose id wraps to that value in `i64`.
        let negative_id_ref = {
            let p = unsafe { ref_heap.alloc_words(4) }.expect("fits");
            // SAFETY: the four words are exclusively owned and written before escaping.
            unsafe {
                *p = make_header(3, REF_SUBTAG);
                *p.add(1) = make_small(0);
                *p.add(2) = make_small(0);
                *p.add(3) = make_small(-1);
            }
            make_boxed(p as *const Eterm)
        };
        let negative_down = heap
            .tuple(&[
                make_atom(down),
                negative_id_ref,
                make_atom(process),
                pid(1).to_eterm(),
                make_small(0),
            ])
            .expect("fits");
        assert!(!payload_is_down_for(negative_down, reference, down));
        assert!(
            !payload_is_down_for(
                negative_down,
                MonitorRef {
                    id: u64::MAX // `-1i64 as u64`
                },
                down
            ),
            "a negative id word must not match a huge id"
        );

        // Non-tuple payloads must never be dereferenced.
        assert!(!payload_is_down_for(make_small(7), reference, down));
        assert!(!payload_is_down_for(make_atom(down), reference, down));
        let not_a_down = heap
            .tuple(&[make_atom(process), make_small(1)])
            .expect("fits");
        assert!(!payload_is_down_for(not_a_down, reference, down));
    }
}
