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

//! The **sources** for every `erlang:process_info/2` item this crate can answer, derived from
//! this crate's public API alone.
//!
//! Oracle ground truth: OTP 29.1.1 / erts-17.1 (`erl -noshell -eval ...`).
//!
//! # `process_info(Pid, Item)` item -> accessor table
//!
//! | item                  | source in this crate's public API                                  | oracle shape / convention                                                                 |
//! |-----------------------|--------------------------------------------------------------------|-------------------------------------------------------------------------------------------|
//! | `status`              | `Process::status()` / `ProcessState::status_atom()`                | an atom `running\|runnable\|waiting\|suspended\|exiting`; the WHOLE call is `undefined` for a dead pid |
//! | `registered_name`     | `ProcessTable::name_of(pid)`                                       | the name atom, or `[]` when unregistered; `undefined` for a dead pid                       |
//! | `message_queue_len`   | `Process::mailbox().len()`                                         | integer                                                                                    |
//! | `links`               | `Process::links().iter()`                                          | `[Pid]`, in link order                                                                     |
//! | `monitors`            | `Process::monitors().iter()`                                       | `[{process, Pid}]` (outgoing process monitors), in insertion order                         |
//! | `monitored_by`        | `Process::monitored_by()`                                          | `[Pid]`, in insertion order                                                                |
//! | `trap_exit`           | `Process::is_trapping_exit()`                                      | boolean                                                                                    |
//! | `priority`            | `Process::priority()` (and `RunQueue::priority_of`)                | `normal\|low\|high\|max`                                                                    |
//! | `reductions`          | `Process::reductions()`                                            | integer                                                                                    |
//!
//! ## `[]` / `undefined` conventions (per the oracle)
//!
//! * `process_info(Pid, registered_name)` is `[]` (**not** `undefined`) when the item exists but
//!   there is no name; it is `undefined` only for a dead pid. `name_of` returns `None` for **both**
//!   cases, so the BIF layer distinguishes them with [`ProcessTable::is_process_alive`]: a live
//!   pid with `None` renders `[]`, a dead pid renders the whole call `undefined`.
//! * `process_info(Pid, status)` returns a `{status, Atom}` tuple only for a live pid; for a dead
//!   pid the entire call is `undefined` (equivalently: `Process::status()` is `None`).
//! * Every other item below is a plain `{Item, Value}` tuple for a live pid and `undefined` for a
//!   dead one (the liveness gate is shared, so it is not re-asserted per item here).
//!
//! ## Scope note: the `{port, Port}` monitor case is separate
//!
//! The `monitors` item **today** covers **process** monitors: [`ProcessTable::monitor`] takes a
//! `ProcessId` target, so [`MonitorKind::Process`] entries are what this API records. A
//! `{port, Port}` entry (a port monitor's outgoing half) is not composed through
//! `ProcessTable::monitor` — a port's incoming half is recorded on `PortTable`/`PortClose`
//! instead — so the port case is deliberately **out of scope here** and this file does not depend
//! on it.
//!
//! There is **no** item above without a truthful source in this crate's public API; each maps to a
//! concrete accessor, asserted below against ONE purpose-built subject process that is linked to
//! another, monitored by another, itself monitors another, has `trap_exit` true, sits at `low`
//! priority, and has queued messages.

use beam_process::prelude::*;
use beam_term::prelude::{make_small, Atom, AtomTable};

/// The purpose-built subject process plus the peers the item sources are read from.
///
/// Composition (all through the public API):
/// * `subject` — `low` priority, `trap_exit` true, two queued messages;
/// * `links[0]`, `links[1]` — linked to `subject` (in that order);
/// * `watchers[0]`, `watchers[1]` — monitor `subject` (so `subject.monitored_by` is both);
/// * `targets[0]`, `targets[1]` — monitored by `subject` (`subject.monitors` is both);
/// * `sender` — the `from` of the two queued messages.
struct Subject {
    table: ProcessTable,
    subject: ProcessId,
    /// The name atom the `registered_name` test interns (registered on demand).
    registered: Atom,
    links: [ProcessId; 2],
    watchers: [ProcessId; 2],
    targets: [ProcessId; 2],
    sender: ProcessId,
}

impl Subject {
    fn new() -> Subject {
        let mut atoms = AtomTable::new();
        let mut table = ProcessTable::new(SignalAtoms::from_table(&mut atoms));
        let registered = atoms.intern("subject");

        // The subject: born-normal, then promoted to LOW with trap_exit set.
        let subject = table.spawn_default().expect("the subject process");
        {
            let process = table.get_mut(subject).expect("the subject is live");
            process.set_priority(Priority::Low);
            process.set_trap_exit(true);
        }

        // Two links, in link order.
        let links = [
            table.spawn_default().expect("link peer 0"),
            table.spawn_default().expect("link peer 1"),
        ];
        for peer in links {
            table.link(subject, peer).expect("both pids are live");
        }

        // Two watchers, in insertion order.
        let watchers = [
            table.spawn_default().expect("watcher 0"),
            table.spawn_default().expect("watcher 1"),
        ];
        for watcher in watchers {
            table
                .monitor(watcher, subject, MonitorKind::Process)
                .expect("the watcher and the subject are live");
        }

        // Two targets the subject monitors, in insertion order.
        let targets = [
            table.spawn_default().expect("target 0"),
            table.spawn_default().expect("target 1"),
        ];
        for target in targets {
            table
                .monitor(subject, target, MonitorKind::Process)
                .expect("the subject and the target are live");
        }

        // Two queued messages.
        let sender = table.spawn_default().expect("sender");
        table
            .deliver_simple(subject, sender.to_eterm(), make_small(1))
            .expect("an immediate always fits");
        table
            .deliver_simple(subject, sender.to_eterm(), make_small(2))
            .expect("an immediate always fits");

        Subject {
            table,
            subject,
            registered,
            links,
            watchers,
            targets,
            sender,
        }
    }

    /// Borrows the subject PCB.
    fn pcb(&self) -> &Process {
        self.table.get(self.subject).expect("the subject is live")
    }

    /// Borrows the subject PCB mutably.
    fn pcb_mut(&mut self) -> &mut Process {
        self.table
            .get_mut(self.subject)
            .expect("the subject is live")
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// `status`: each `ProcessState` has its own atom, and `Terminated` has none (a dead pid makes the
/// whole `process_info` call `undefined`).
#[test]
fn status_item_pins_each_state_atom_and_is_none_for_terminated() {
    // Every state except `Terminated` maps to the oracle's own atom.
    assert_eq!(ProcessState::Running.status_atom(), Some("running"));
    assert_eq!(ProcessState::Runnable.status_atom(), Some("runnable"));
    assert_eq!(ProcessState::Waiting.status_atom(), Some("waiting"));
    assert_eq!(ProcessState::Suspended.status_atom(), Some("suspended"));
    assert_eq!(ProcessState::Exiting.status_atom(), Some("exiting"));
    // A terminated process has no status: the BIF renders the whole call `undefined`.
    assert_eq!(ProcessState::Terminated.status_atom(), None);

    let mut fixture = Subject::new();

    // A freshly spawned process has not run yet: `runnable`.
    assert_eq!(fixture.pcb().state(), ProcessState::Runnable);
    assert_eq!(fixture.pcb().status(), Some("runnable"));

    // Blocked in a `receive`: `waiting`.
    fixture.pcb_mut().set_state(ProcessState::Waiting);
    assert_eq!(fixture.pcb().status(), Some("waiting"));

    // Suspended from outside.
    fixture.pcb_mut().set_state(ProcessState::Suspended);
    assert_eq!(fixture.pcb().status(), Some("suspended"));

    // The transient GC status overrides the scheduler state while it is set.
    fixture.pcb_mut().flags_mut().set_garbage_collecting(true);
    assert_eq!(fixture.pcb().status(), Some("garbage_collecting"));
    fixture.pcb_mut().flags_mut().set_garbage_collecting(false);
    assert_eq!(fixture.pcb().status(), Some("suspended"));

    // A terminated process -> `None`, i.e. the whole call is `undefined` for a dead pid.
    fixture.pcb_mut().set_state(ProcessState::Terminated);
    assert_eq!(fixture.pcb().status(), None);
}

// ---------------------------------------------------------------------------
// registered_name
// ---------------------------------------------------------------------------

/// `registered_name`: `None` renders `[]` for a live unregistered pid and `undefined` for a dead
/// one; the registered name atom answers once `register` succeeds.
#[test]
fn registered_name_item_is_none_then_the_atom() {
    let mut fixture = Subject::new();
    let name = fixture.registered;

    // Unregistered (but live) -> `[]`.
    assert_eq!(fixture.table.name_of(fixture.subject), None);
    assert!(fixture.table.is_process_alive(fixture.subject));

    // Registered -> the atom.
    fixture
        .table
        .register(name, fixture.subject)
        .expect("the name is free");
    assert_eq!(fixture.table.name_of(fixture.subject), Some(name));

    // A dead pid reports no name *and* is not alive: the BIF renders the whole call `undefined`.
    let dead = fixture.table.spawn_default().expect("a process about to die");
    fixture.table.remove(dead).expect("it was live");
    assert_eq!(fixture.table.name_of(dead), None);
    assert!(!fixture.table.is_process_alive(dead));
}

// ---------------------------------------------------------------------------
// message_queue_len
// ---------------------------------------------------------------------------

/// `message_queue_len`: exactly the mailbox length, in FIFO order.
#[test]
fn message_queue_len_item_is_the_mailbox_len() {
    let fixture = Subject::new();
    assert_eq!(fixture.pcb().mailbox().len(), 2);

    let payloads: Vec<_> = fixture.pcb().mailbox().iter().map(|m| m.payload()).collect();
    assert_eq!(payloads, vec![make_small(1), make_small(2)]);
    assert_eq!(fixture.pcb().mailbox().iter().next().unwrap().from(), fixture.sender.to_eterm());

    // The count tracks a drain one message at a time.
    let mut fixture = fixture;
    let mut received = 0;
    while fixture.pcb_mut().receive(|_| true).is_some() {
        received += 1;
    }
    assert_eq!(received, 2);
    assert_eq!(fixture.pcb().mailbox().len(), 0);
}

// ---------------------------------------------------------------------------
// links
// ---------------------------------------------------------------------------

/// `links`: `[Pid]` in link order (and symmetric on each peer).
#[test]
fn links_item_lists_linked_pids_in_link_order() {
    let fixture = Subject::new();
    let links: Vec<ProcessId> = fixture.pcb().links().iter().collect();
    assert_eq!(links, fixture.links.to_vec(), "`[Pid]` in link order");

    for peer in fixture.links {
        assert!(
            fixture
                .table
                .get(peer)
                .expect("the peer is live")
                .links()
                .contains(fixture.subject),
            "the link is symmetric"
        );
    }
}

// ---------------------------------------------------------------------------
// monitors (outgoing)
// ---------------------------------------------------------------------------

/// `monitors`: `[{process, Pid}]` for the subject's **outgoing** process monitors, in insertion
/// order, each with a distinct reference.
#[test]
fn monitors_item_lists_outgoing_process_monitors_in_insertion_order() {
    let fixture = Subject::new();
    let entries: Vec<(MonitorRef, MonitorKind, ProcessId)> = fixture
        .pcb()
        .monitors()
        .iter()
        .map(|m| (m.reference, m.kind, m.target))
        .collect();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].1, MonitorKind::Process, "the `process` tag");
    assert_eq!(entries[0].2, fixture.targets[0]);
    assert_eq!(entries[1].1, MonitorKind::Process);
    assert_eq!(entries[1].2, fixture.targets[1]);
    assert_ne!(entries[0].0, entries[1].0, "each monitor has its own reference");

    // The incoming half is recorded on each target.
    for (index, target) in fixture.targets.iter().enumerate() {
        let watched_by = fixture
            .table
            .get(*target)
            .expect("the target is live")
            .monitored_by();
        assert_eq!(watched_by.len(), 1);
        assert_eq!(watched_by[0].watcher, fixture.subject);
        assert_eq!(watched_by[0].kind, MonitorKind::Process);
        assert_eq!(watched_by[0].reference, entries[index].0);
    }
}

// ---------------------------------------------------------------------------
// monitored_by (incoming)
// ---------------------------------------------------------------------------

/// `monitored_by`: `[Pid]` for the subject's watchers, in insertion order.
#[test]
fn monitored_by_item_lists_watchers_in_insertion_order() {
    let fixture = Subject::new();
    let watchers: Vec<ProcessId> = fixture
        .pcb()
        .monitored_by()
        .iter()
        .map(|m| m.watcher)
        .collect();
    assert_eq!(watchers, fixture.watchers.to_vec(), "`[Pid]` in insertion order");

    // The subject's own outgoing monitors do not leak into `monitored_by`.
    assert_eq!(fixture.pcb().monitors().len(), 2);
    assert_eq!(fixture.pcb().monitored_by().len(), 2);
}

// ---------------------------------------------------------------------------
// trap_exit
// ---------------------------------------------------------------------------

/// `trap_exit`: the PCB flag; a freshly spawned peer defaults to `false`.
#[test]
fn trap_exit_item_is_the_pcb_flag_and_defaults_false() {
    let fixture = Subject::new();
    assert!(fixture.pcb().is_trapping_exit(), "the subject traps exits");
    for peer in fixture.links {
        assert!(
            !fixture
                .table
                .get(peer)
                .expect("the peer is live")
                .is_trapping_exit(),
            "a freshly spawned peer does not trap exits"
        );
    }
}

// ---------------------------------------------------------------------------
// priority
// ---------------------------------------------------------------------------

/// `priority`: the PCB priority (`low` for the subject), agreeing with the run queue for a queued
/// pid, and covering the oracle's whole `normal|low|high|max` atom set.
#[test]
fn priority_item_is_the_pcb_priority_and_reaches_the_run_queue() {
    let mut fixture = Subject::new();
    assert_eq!(fixture.pcb().priority(), Priority::Low);

    // The oracle's atom set.
    assert_eq!(Priority::Max.name(), "max");
    assert_eq!(Priority::High.name(), "high");
    assert_eq!(Priority::Normal.name(), "normal");
    assert_eq!(Priority::Low.name(), "low");

    // A queued pid reports the same level through the run queue.
    let mut scheduler = Scheduler::new();
    scheduler.enqueue_with_priority(fixture.subject, Priority::Low);
    assert_eq!(
        scheduler.run_queue().priority_of(fixture.subject),
        Some(Priority::Low)
    );

    // And a promotion made on the PCB is what the accessor then reports.
    fixture.pcb_mut().set_priority(Priority::High);
    assert_eq!(fixture.pcb().priority(), Priority::High);
}

// ---------------------------------------------------------------------------
// reductions
// ---------------------------------------------------------------------------

/// `reductions`: the PCB reduction counter.
#[test]
fn reductions_item_is_the_pcb_counter() {
    let mut fixture = Subject::new();
    assert_eq!(fixture.pcb().reductions(), 0);

    fixture.pcb_mut().bump_reductions(7);
    fixture.pcb_mut().bump_reductions(35);
    assert_eq!(fixture.pcb().reductions(), 42);

    fixture.pcb_mut().reset_reductions();
    assert_eq!(fixture.pcb().reductions(), 0);
}
