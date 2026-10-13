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

//! The decoupled per-process pending-signal queue: [`SignalQueue`] and [`PendingSignal`].
//!
//! ERTS does **not** apply an exit signal, a link exit or a monitor `{'DOWN'}` at the instant
//! it is generated: it *queues* the signal on the receiving process and applies it later, at a
//! well-defined point (legacy C `erl_proc_sig_queue.c`): a process's own queue is drained when
//! it is scheduled in, and a signal to a process that is **not** currently executing is
//! handled eagerly at the end of the sending operation (ERTS's early signal handling). This
//! module is that per-process queue; the application policy lives in
//! [`crate::ptab::ProcessTable::service_signals`].
//!
//! # What a queued signal is
//!
//! A signal is either an **exit signal** — `from: ProcessId`, `reason: Eterm`, `family`
//! ([`ExitFamily`]: an explicit `erlang:exit/2` signal or a link exit, which decides whether
//! `kill` is untrappable) — or a
//! **monitor-DOWN** notification — `reference`, `target`, `kind`, an optional **user tag**,
//! `reason` ([`PendingSignal`]). Both carry the exit/monitor `reason` as a live term: the
//! reason is **copied into the receiving process's own heap when the signal is enqueued** (see
//! below), so the queue is an in-heap sub-rootset exactly like the mailbox / the process
//! dictionary / the saved BIF arguments. The DOWN's user tag is the `erlang:monitor/3`
//! `{tag, Tag}` option (`None` ⇒ the `'DOWN'` atom) and is carried verbatim — it must be a
//! heap-independent immediate (see `monitor.rs`). A remote (external-pid) or port signal is
//! *not* queued: it is materialised straight into the mailbox, because it carries no
//! `trap_exit` policy decision to defer.
//!
//! # Ordering guarantee
//!
//! **Per receiver, FIFO.** Every signal enqueued for one process is applied in the exact order
//! it was enqueued (a single FIFO per process). Because the runtime is single-threaded and a
//! signal is enqueued at the moment it is *generated*, this gives the ERTS observable
//! guarantee for a signalling **pair**: signals from one sender to one receiver are applied in
//! the order they were sent (the per-`(sender, receiver)` ordering of
//! `erl_proc_sig_queue.c`), and it is in fact stronger — it is a total order per receiver.
//!
//! # What is *not* guaranteed (documented honestly)
//!
//! * **No cross-ordering between the signal queue and the mailbox.** A `{'EXIT', …}`/
//!   `{'DOWN', …}` signal becomes a mailbox message only when it is *applied*; an ordinary
//!   message sent after the signal was generated can therefore be enqueued on the mailbox
//!   first. ERTS's real signal queue tightens this by flushing the queue before the process
//!   runs; this crate applies it at schedule-in, which fixes the end state but not the
//!   interleaving with concurrent sends.
//! * **No ordering between different receivers.** Two signals to two different processes may
//!   be applied in any order (each queue is drained independently).
//!
//! # Term ownership (the GC contract)
//!
//! A queued reason term lives in the receiving process's **own** young heap (it is copied in
//! at enqueue time through the crate's existing copy-into-heap machinery). It is therefore an
//! **extra in-heap sub-rootset** the copying collector must scan: [`SignalQueue::gather_roots`]
//! / [`SignalQueue::scatter_roots`] are that seam, and [`crate::process::Process::collect`]
//! calls them in the same place it gathers the dictionary and the saved arguments. Monitor
//! references are **not** on-heap — a [`MonitorRef`]/[`ProcessId`] is stored by value and the
//! `Ref`/pid terms are built on the heap only when the signal is applied.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_proc_sig_queue.c` (the signal
//! queue, its ordering and its serviced-at-schedule-in rule) and
//! `erts/emulator/beam/erl_monitor_link.c`.

use std::collections::VecDeque;

use beam_term::prelude::Eterm;

use crate::monitor::{MonitorKind, MonitorRef};
use crate::pid::ProcessId;
use crate::signal::ExitFamily;

/// What kind of signal a [`PendingSignal`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingSignalKind {
    /// An exit signal: `erlang:exit/2`, link propagation, or a node/port-close cascade.
    Exit,
    /// A monitor `{'DOWN', …}` notification for a watched process going down.
    Down,
}

/// A signal waiting to be applied to a process.
///
/// The `reason` of either variant is a term resident in the **receiving** process's own heap
/// (see the module docs); every other field is a plain value. The type is `Copy`, so a
/// processor may peek it, decide the policy, and still re-read the (possibly relocated) head
/// after a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSignal {
    /// An exit signal from `from` carrying `reason`, in the [`ExitFamily`] that decides its trap
    /// rule (`kill` is untrappable only in the explicit `erlang:exit/2` family).
    Exit {
        /// The signalling process.
        from: ProcessId,
        /// The exit reason term (already owned by the receiver's heap).
        reason: Eterm,
        /// The signal family: an explicit `erlang:exit/2` signal, or a link exit.
        family: ExitFamily,
    },
    /// A monitor-DOWN notification: the watched `target` is down with `reason`.
    Down {
        /// The monitor reference the `{'DOWN', …}` message must carry.
        reference: MonitorRef,
        /// The watched process (downgraded to a `{'DOWN', …}` payload at service time).
        target: ProcessId,
        /// Whether a process or a port is watched (selects the `{'DOWN', …}` kind atom).
        kind: MonitorKind,
        /// The `erlang:monitor/3` `{tag, Tag}` user tag; `None` ⇒ the `'DOWN'` atom.
        tag: Option<Eterm>,
        /// The exit reason term (already owned by the receiver's heap).
        reason: Eterm,
    },
}

impl PendingSignal {
    /// An exit signal from `from` with `reason`, belonging to `family`.
    pub fn exit(from: ProcessId, reason: Eterm, family: ExitFamily) -> PendingSignal {
        PendingSignal::Exit {
            from,
            reason,
            family,
        }
    }

    /// A monitor-DOWN notification, with the optional `erlang:monitor/3` user tag
    /// (`None` ⇒ the `'DOWN'` atom).
    pub fn down(
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
        reason: Eterm,
    ) -> PendingSignal {
        PendingSignal::Down {
            reference,
            target,
            kind,
            tag,
            reason,
        }
    }

    /// The kind of this signal (exit or monitor-DOWN).
    pub fn kind(&self) -> PendingSignalKind {
        match self {
            PendingSignal::Exit { .. } => PendingSignalKind::Exit,
            PendingSignal::Down { .. } => PendingSignalKind::Down,
        }
    }

    /// The reason term this signal carries.
    pub fn reason(&self) -> Eterm {
        match self {
            PendingSignal::Exit { reason, .. } | PendingSignal::Down { reason, .. } => *reason,
        }
    }

    /// The monitor-DOWN's user tag, or `None` for an exit signal / a default-tag DOWN.
    pub fn tag(&self) -> Option<Eterm> {
        match self {
            PendingSignal::Exit { .. } => None,
            PendingSignal::Down { tag, .. } => *tag,
        }
    }

    /// Replaces the reason term (used by the GC `scatter` step to write the relocated word
    /// back).
    fn set_reason(&mut self, reason: Eterm) {
        match self {
            PendingSignal::Exit { reason: slot, .. } | PendingSignal::Down { reason: slot, .. } => {
                *slot = reason
            }
        }
    }

    /// Replaces the DOWN's user tag (the GC `scatter` step, mirroring [`PendingSignal::set_reason`]).
    fn set_tag(&mut self, tag: Eterm) {
        if let PendingSignal::Down { tag: slot, .. } = self {
            *slot = Some(tag);
        }
    }
}

/// The per-process FIFO of pending signals (see the module docs for the ordering guarantee and
/// the GC contract).
#[derive(Default)]
pub struct SignalQueue {
    entries: VecDeque<PendingSignal>,
}

impl SignalQueue {
    /// An empty signal queue.
    pub fn new() -> SignalQueue {
        SignalQueue {
            entries: VecDeque::new(),
        }
    }

    /// Appends `signal` at the **back** of the queue (the order it will be applied in).
    pub fn push(&mut self, signal: PendingSignal) {
        self.entries.push_back(signal);
    }

    /// Removes and returns the **oldest** queued signal, or `None` when the queue is empty.
    pub fn pop(&mut self) -> Option<PendingSignal> {
        self.entries.pop_front()
    }

    /// Borrows the oldest queued signal without removing it.
    pub fn peek(&self) -> Option<&PendingSignal> {
        self.entries.front()
    }

    /// Removes every queued signal, returning them in application order.
    pub fn drain(&mut self) -> Vec<PendingSignal> {
        self.entries.drain(..).collect()
    }

    /// Removes every queued signal.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The number of queued signals.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue holds no signals.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates the queued signals in application (FIFO) order.
    pub fn iter(&self) -> impl Iterator<Item = &PendingSignal> + '_ {
        self.entries.iter()
    }

    /// Appends every queued signal's reason (in item order) — plus each monitor-DOWN signal's
    /// user tag when it has one — to `out` and returns the **start offset** — the index in
    /// `out` at which this container's words begin.
    ///
    /// One word per signal, plus one more for a DOWN carrying a user tag; pairing with
    /// [`SignalQueue::scatter_roots`] with the same `start` makes the two a clean round-trip
    /// (mirroring [`crate::dict::Dict::gather_roots`]). The `Some`/`None` shape of every DOWN's
    /// tag is identical at gather and scatter time (an immediate never gains or loses a tag), so
    /// the positional pairing is exact.
    pub fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        let start = out.len();
        for signal in &self.entries {
            out.push(signal.reason());
            if let Some(tag) = signal.tag() {
                out.push(tag);
            }
        }
        start
    }

    /// Writes the (relocated) reason and user-tag words back, starting at `start` — the offset
    /// returned by [`SignalQueue::gather_roots`].
    ///
    /// Defensive and total: it never panics. A `roots` slice too short to cover the whole
    /// queue, or a `start` past its end, simply leaves the uncovered tail untouched.
    pub fn scatter_roots(&mut self, roots: &[Eterm], start: usize) {
        let mut index = start;
        for signal in self.entries.iter_mut() {
            let Some(reason) = roots.get(index) else {
                // `start` is past the end of the slice (or it ran out): nothing more to write.
                return;
            };
            signal.set_reason(*reason);
            index += 1;
            if signal.tag().is_some() {
                let Some(tag) = roots.get(index) else {
                    return;
                };
                signal.set_tag(*tag);
                index += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the FIFO ordering, the drain/clear semantics, the introspection
    //! accessors and the `gather_roots`/`scatter_roots` round-trip.

    use super::*;
    use beam_term::prelude::make_small;

    fn pid(number: u64) -> ProcessId {
        ProcessId::new(number, 0)
    }

    /// A queued exit signal in the **explicit** `erlang:exit/2` family (the tests' default; the
    /// queue stores the family verbatim and the trap rule is applied by the process table).
    fn exit(from: ProcessId, reason: Eterm) -> PendingSignal {
        PendingSignal::exit(from, reason, ExitFamily::Explicit)
    }
    fn reference(id: u64) -> MonitorRef {
        // The allocator is the only constructor; use it for a real id.
        let mut allocator = crate::monitor::RefAllocator::new();
        for _ in 0..id {
            allocator.next();
        }
        allocator.next()
    }

    #[test]
    fn a_fresh_queue_is_empty() {
        let queue = SignalQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert!(queue.peek().is_none());
        assert!(queue.iter().next().is_none());
    }

    #[test]
    fn push_pop_is_fifo() {
        let mut queue = SignalQueue::new();
        queue.push(exit(pid(1), make_small(1)));
        queue.push(exit(pid(2), make_small(2)));
        queue.push(PendingSignal::down(
            reference(0),
            pid(3),
            MonitorKind::Process,
            None,
            make_small(3),
        ));
        assert_eq!(queue.len(), 3);

        assert_eq!(
            queue.peek().copied(),
            Some(exit(pid(1), make_small(1)))
        );
        assert_eq!(
            queue.pop(),
            Some(exit(pid(1), make_small(1)))
        );
        assert_eq!(
            queue.pop(),
            Some(exit(pid(2), make_small(2)))
        );
        assert_eq!(queue.pop().map(|s| s.kind()), Some(PendingSignalKind::Down));
        assert!(queue.is_empty());
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn drain_returns_the_signals_in_application_order() {
        let mut queue = SignalQueue::new();
        queue.push(exit(pid(1), make_small(1)));
        queue.push(exit(pid(2), make_small(2)));
        let drained = queue.drain();
        assert_eq!(
            drained,
            vec![
                exit(pid(1), make_small(1)),
                exit(pid(2), make_small(2)),
            ]
        );
        assert!(queue.is_empty());
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn introspection_exposes_kind_and_reason() {
        let signal = exit(pid(4), make_small(41));
        assert_eq!(signal.kind(), PendingSignalKind::Exit);
        assert_eq!(signal.reason(), make_small(41));

        let reference = reference(2);
        let down = PendingSignal::down(reference, pid(5), MonitorKind::Process, None, make_small(51));
        assert_eq!(down.kind(), PendingSignalKind::Down);
        assert_eq!(down.reason(), make_small(51));
        // No user tag ⇒ the default `'DOWN'` head.
        assert_eq!(down.tag(), None);
        match down {
            PendingSignal::Down {
                reference: got,
                target,
                kind,
                ..
            } => {
                assert_eq!(got, reference);
                assert_eq!(target, pid(5));
                assert_eq!(kind, MonitorKind::Process);
            }
            PendingSignal::Exit { .. } => panic!("a DOWN signal must be a Down"),
        }

        // A user tag is carried verbatim by the signal.
        let tagged = PendingSignal::down(reference, pid(6), MonitorKind::Port, Some(make_small(7)), make_small(52));
        assert_eq!(tagged.tag(), Some(make_small(7)));
        assert_eq!(tagged.reason(), make_small(52));
        // An exit signal has no tag.
        assert_eq!(exit(pid(1), make_small(1)).tag(), None);
    }

    #[test]
    fn an_exit_signal_carries_its_family() {
        // The queue stores the family verbatim; the trap rule that consumes it is the table's
        // (`exit_policy`), so a link exit must not be silently downgraded to the explicit family.
        let explicit = exit(pid(1), make_small(1));
        let linked = PendingSignal::exit(pid(1), make_small(1), ExitFamily::Linked);
        assert!(matches!(
            explicit,
            PendingSignal::Exit {
                family: ExitFamily::Explicit,
                ..
            }
        ));
        assert!(matches!(
            linked,
            PendingSignal::Exit {
                family: ExitFamily::Linked,
                ..
            }
        ));
        // Only the family differs, so the two signals are not the same signal.
        assert_ne!(explicit, linked);
    }

    #[test]
    fn clear_empties_the_queue() {
        let mut queue = SignalQueue::new();
        queue.push(exit(pid(1), make_small(1)));
        queue.clear();
        assert!(queue.is_empty());
    }

    #[test]
    fn iter_yields_in_application_order() {
        let mut queue = SignalQueue::new();
        queue.push(exit(pid(1), make_small(1)));
        queue.push(exit(pid(2), make_small(2)));
        let reasons: Vec<i64> = queue.iter().map(|s| signed(s.reason())).collect();
        assert_eq!(reasons, vec![1, 2]);
    }

    #[test]
    fn gather_and_scatter_round_trip() {
        let mut queue = SignalQueue::new();
        queue.push(exit(pid(1), make_small(1)));
        queue.push(PendingSignal::down(
            reference(0),
            pid(2),
            MonitorKind::Process,
            None,
            make_small(2),
        ));

        let mut roots = vec![make_small(-1)]; // a caller-owned prefix is preserved
        let start = queue.gather_roots(&mut roots);
        assert_eq!(start, 1);
        assert_eq!(&roots[start..], &[make_small(1), make_small(2)]);

        // Simulate the collector relocating every root.
        roots[start..].copy_from_slice(&[make_small(101), make_small(202)]);
        queue.scatter_roots(&roots, start);
        assert_eq!(queue.pop().map(|s| s.reason()), Some(make_small(101)));
        assert_eq!(queue.pop().map(|s| s.reason()), Some(make_small(202)));
    }

    #[test]
    fn gather_and_scatter_carry_a_down_user_tag() {
        let mut queue = SignalQueue::new();
        queue.push(PendingSignal::down(
            reference(0),
            pid(2),
            MonitorKind::Process,
            Some(make_small(7)),
            make_small(2),
        ));
        // An untagged DOWN next to a tagged one: the tagged one contributes an extra word.
        queue.push(PendingSignal::down(
            reference(1),
            pid(3),
            MonitorKind::Port,
            None,
            make_small(3),
        ));

        let mut roots = Vec::new();
        let start = queue.gather_roots(&mut roots);
        assert_eq!(start, 0);
        // [reason, tag, reason] — the untagged DOWN contributes only its reason.
        assert_eq!(roots, vec![make_small(2), make_small(7), make_small(3)]);

        // The collector relocates both the reason and the tag.
        queue.scatter_roots(&[make_small(102), make_small(107), make_small(103)], start);
        let first = queue.pop().expect("the tagged DOWN");
        assert_eq!(first.reason(), make_small(102));
        assert_eq!(first.tag(), Some(make_small(107)));
        let second = queue.pop().expect("the untagged DOWN");
        assert_eq!(second.reason(), make_small(103));
        assert_eq!(second.tag(), None);
    }

    #[test]
    fn gather_of_an_empty_queue_returns_the_current_len() {
        let queue = SignalQueue::new();
        let mut roots = vec![make_small(7)];
        assert_eq!(queue.gather_roots(&mut roots), 1);
        assert_eq!(roots, vec![make_small(7)]);
    }

    #[test]
    fn scatter_is_total_for_a_mismatched_slice() {
        let mut queue = SignalQueue::new();
        queue.push(exit(pid(1), make_small(1)));
        queue.push(exit(pid(2), make_small(2)));

        // A slice covering only the first signal leaves the second untouched.
        queue.scatter_roots(&[make_small(101)], 0);
        assert_eq!(queue.pop().map(|s| s.reason()), Some(make_small(101)));
        assert_eq!(queue.pop().map(|s| s.reason()), Some(make_small(2)));

        // A `start` past the end is likewise a no-op.
        let mut queue = SignalQueue::new();
        queue.push(exit(pid(3), make_small(3)));
        queue.scatter_roots(&[make_small(9)], 99);
        assert_eq!(queue.pop().map(|s| s.reason()), Some(make_small(3)));
    }

    /// The small-int value of an immediate term (test helper).
    fn signed(term: Eterm) -> i64 {
        beam_term::prelude::signed_value(term)
    }
}
