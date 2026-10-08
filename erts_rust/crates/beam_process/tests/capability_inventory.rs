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

//! Capability inventory: every deliberately-**unsupported** or **half-modelled** path of this
//! crate's process/port control surface, pinned with its exact observable.
//!
//! The crate `CONTEXT.md` mandates that an unimplemented capability returns a **typed** error
//! (`crate::error::unsupported`, a BEAM `error` whose reason is `unsupported: <what>`) or the
//! error the oracle itself raises — **never** a wrong result, a panic or a hang. This file is the
//! executable statement of that contract: each check asserts the exact [`BeamError`] (matching the
//! variant *and* the atom reason where one exists) or the exact documented record-only behaviour.
//!
//! # What is pinned here
//!
//! * **objective item 2** — a **name** monitor target without an installed local node
//!   (`erlang:monitor(process, Name)`): the typed `unsupported: monitor/2: name target requires the
//!   local node (set_local_node)`, and `ProcessTable::set_local_node` changing the outcome.
//! * **objective item 6** — the `erlang:monitor/2` **type dispatch**: `time_offset` (removed in
//!   OTP 26) and every other atom are not a monitor type (`MonitorKind::from_type_name` ⇒ `None`,
//!   which the BIF layer renders as the oracle's `badarg`). There is no `TimeOffset` variant, so
//!   no wrong result is reachable.
//! * **objective item 8** — the non-pid/non-atom/non-port target ⇒ `badarg`;
//!   `erlang:monitor(port, NotAPort)` ⇒ `badarg`; the **external-pid** operations with **no**
//!   `DistSignalRouter` (record-only, never a fabricated message); and a relationship mutation on
//!   an **unallocated** port ⇒ `badarg` with no side effect.
//! * The `monitor(port, Port)` surface itself (objective item A1): both halves recorded, the
//!   watcher's outgoing [`PortWatch`] resolved by `port_closed` and removable by
//!   `demonitor_port`, and the born-down `noproc` shape for a port that is already gone — the
//!   oracle's behaviour, not a `badarg`.
//! * The audit fact that the outgoing port-watch set is **not** a collector root.
//!
//! # Oracle cross-checks
//!
//! The `oracle_*` checks drive the real system Erlang (OTP 29.1.1 / erts-17.1, `erl` on `PATH`)
//! with the same text protocol `tests/registry_monitors.rs` uses and assert its printed answer.
//! **Absence of the oracle is not a failure**: if `erl` cannot be spawned or does not answer
//! within [`ERL_TIMEOUT`], the check skips loudly and returns.

use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_process::prelude::*;
use beam_term::prelude::{
    external_pid_size_words, make_atom, make_small, tuple_element, tuple_size, write_external_pid,
    Atom, AtomTable, Eterm,
};

/// Wall-clock budget for a single `erl` invocation before the oracle is declared unusable and the
/// affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The oracle driver (text protocol, shared with `tests/registry_monitors.rs`)
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout.
///
/// Returns `None` — after a clear skip message — when `erl` cannot be spawned or does not answer
/// within [`ERL_TIMEOUT`]. A *spawned* `erl` that exits non-zero is a bug in the embedded
/// expression, so that is asserted rather than skipped.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        // The receiver may have timed out and gone away; that is fine.
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "`erl` exited with {} for expression:\n{code}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Runs `expr` under the oracle and asserts the printed result is `expected`.
fn expect_oracle(expr: &str, expected: &str) -> bool {
    let Some(out) = erl_stdout(expr) else {
        return false;
    };
    assert_eq!(
        out.trim(),
        expected,
        "the oracle disagrees with the expected result for:\n{expr}"
    );
    true
}

// ---------------------------------------------------------------------------
// Fixtures and assertions
// ---------------------------------------------------------------------------

/// A process table plus the atom table its [`SignalAtoms`] were interned from, and a scratch heap
/// for building external-pid terms.
struct Fixture {
    table: ProcessTable,
    names: AtomTable,
    heap: Heap,
}

impl Fixture {
    /// An empty table whose atoms come from a fresh table the fixture keeps.
    fn new() -> Fixture {
        let mut names = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut names);
        Fixture {
            table: ProcessTable::new(atoms),
            names,
            heap: Heap::new(256).expect("a scratch heap for external-pid terms"),
        }
    }

    /// The interned atom `name` (the same index space the table's signal atoms use).
    fn atom(&mut self, name: &str) -> Atom {
        self.names.intern(name)
    }

    /// The atom `name` as a term.
    fn atom_term(&mut self, name: &str) -> Eterm {
        make_atom(self.atom(name))
    }

    /// Builds a tuple on `pid`'s own heap (a *structured* term for the process that owns it).
    fn tuple_in(&mut self, pid: ProcessId, elems: &[Eterm]) -> Eterm {
        self.table
            .get_mut(pid)
            .expect("the holder is live")
            .heap_mut()
            .tuple(elems)
            .expect("the tuple fits the holder's young heap")
    }

    /// Builds an external pid term (the frozen 4-word `[header, node, creation, data]` thing) on
    /// the fixture's scratch heap.
    fn remote_pid(&mut self, node: Eterm, number: u32, serial: u32, creation: u32) -> Eterm {
        // SAFETY: `alloc_words` hands out `external_pid_size_words()` (4) exclusively-owned,
        // writable words.
        let p = unsafe {
            self.heap
                .alloc_words(external_pid_size_words())
                .expect("the scratch heap has room")
        };
        // SAFETY: `p` addresses the four words just handed out and `write_external_pid` writes all
        // of them before the pointer escapes this block; `node` is a live atom term.
        unsafe { write_external_pid(p, node, number, serial, creation) }
    }
}

/// Asserts `err` is the typed `unsupported: <expected>` raise.
///
/// This is the crate's single way to report a capability gap, so the check pins both the shape
/// (`BeamError::Raise`) and the exact reason atom — a `badarg`/`TermNotFound` substitute would slip
/// a wrong error class past a caller.
fn assert_typed_unsupported(err: BeamError, expected: &str) {
    match err {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some(expected),
            "the typed unsupported raise must name the missing capability exactly"
        ),
        other => panic!("expected the typed `unsupported` raise, got {other:?}"),
    }
}

/// The payload of `pid`'s single queued message (panics unless exactly one is queued).
fn sole_payload(table: &ProcessTable, pid: ProcessId) -> Eterm {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued").payload()
}

/// The one `unsupported:` reason naming a **named port** monitor (shared by the two spellings).
const NAMED_PORT_MONITOR: &str =
    "unsupported: monitor/2: named port monitor (the local name registry holds processes only)";

/// The one `unsupported:` reason naming a name target without an installed local node.
const NAME_NEEDS_LOCAL_NODE: &str =
    "unsupported: monitor/2: name target requires the local node (set_local_node)";

/// The one `unsupported:` reason naming a port target given to the pid/name entry point.
const PORT_NEEDS_PORT_TABLE: &str =
    "unsupported: monitor/2: port target requires the port table (use monitor_port)";

// ---------------------------------------------------------------------------
// objective item 2 — a name target needs the local node
// ---------------------------------------------------------------------------

#[test]
fn monitor_of_a_name_without_a_local_node_is_a_typed_unsupported() {
    // Pins objective item 2: `erlang:monitor(process, Name)`'s DOWN target is `{Name, LocalNode}`,
    // and only the runtime can install the node atom. Until then the call is the typed
    // `unsupported`, never a guessed node and never a recorded monitor.
    let mut f = Fixture::new();
    let name = f.atom("server");
    let node = f.atom("nonode@nohost");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let target = f.table.spawn_default().expect("a target process");
    f.table.register(name, target).expect("the name is free");
    let name_term = make_atom(name);

    assert_eq!(f.table.local_node(), None, "no local node is installed yet");

    assert_typed_unsupported(
        f.table
            .monitor_term(watcher, name_term, MonitorKind::Process)
            .expect_err("a name target without the local node cannot be resolved"),
        NAME_NEEDS_LOCAL_NODE,
    );

    // A typed error, not a partial monitor: nothing on the watcher, the target or the mailbox.
    let watcher_process = f.table.get(watcher).expect("the watcher is live");
    assert!(watcher_process.monitors().is_empty());
    assert!(watcher_process.port_monitors().is_empty());
    assert!(watcher_process.mailbox().is_empty());
    assert!(f
        .table
        .get(target)
        .expect("the target is live")
        .monitored_by()
        .is_empty());

    // Installing the node changes the outcome: the same call now takes the named-monitor path.
    f.table.set_local_node(node);
    assert_eq!(f.table.local_node(), Some(node));
    let reference = f
        .table
        .monitor_term(watcher, name_term, MonitorKind::Process)
        .expect("the name resolves once the node is installed");
    assert_eq!(
        f.table
            .get(watcher)
            .expect("the watcher is live")
            .monitors()
            .find(reference)
            .map(|monitor| monitor.target),
        Some(target)
    );
    assert_eq!(
        f.table
            .get(target)
            .expect("the target is live")
            .monitored_by()
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// objective item 6 — the `erlang:monitor/2` type dispatch
// ---------------------------------------------------------------------------

#[test]
fn monitor_type_dispatch_maps_only_process_and_port() {
    // Pins objective item 6: `time_offset` (a monitor type OTP removed in OTP 26) and every other
    // atom are **not** a monitor type — `None`, the value the BIF layer renders as the oracle's
    // `badarg`. `MonitorKind` has no `TimeOffset` variant, so no wrong result is reachable.
    assert_eq!(MonitorKind::from_type_name("time_offset"), None);
    assert_eq!(MonitorKind::from_type_name("bogus_type"), None);
    assert_eq!(MonitorKind::from_type_name(""), None);
    assert_eq!(
        MonitorKind::from_type_name("Process"),
        None,
        "the oracle's atom is the exact lowercase name"
    );
    assert_eq!(
        MonitorKind::from_type_name("process"),
        Some(MonitorKind::Process)
    );
    assert_eq!(MonitorKind::from_type_name("port"), Some(MonitorKind::Port));
    assert_eq!(MonitorKind::Process.type_name(), "process");
    assert_eq!(MonitorKind::Port.type_name(), "port");
    // The one `port` type name that is *not* accepted is the removed `time_offset`.
    for kind in [MonitorKind::Process, MonitorKind::Port] {
        assert_ne!(kind.type_name(), "time_offset");
        assert_eq!(MonitorKind::from_type_name(kind.type_name()), Some(kind));
    }
}

// ---------------------------------------------------------------------------
// objective item 8 — bad targets and the unallocated-port surface
// ---------------------------------------------------------------------------

#[test]
fn monitor_of_a_bad_target_is_badarg_and_a_port_target_is_typed() {
    // Pins objective item 8: a term that is neither a pid, a port nor an atom is the oracle's
    // `badarg`; `erlang:monitor(port, NotAPort)` is `badarg`; and a **port** target handed to the
    // pid/name entry point (which has no `PortTable`) is the typed capability gap, never a
    // fabricated monitor.
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();

    // A tuple and an integer target are `badarg` (the oracle's reason).
    let tuple = f.tuple_in(watcher, &[make_small(1), make_small(2)]);
    assert!(matches!(
        f.table.monitor_term(watcher, tuple, MonitorKind::Process),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        f.table.monitor_term(watcher, make_small(1), MonitorKind::Process),
        Err(BeamError::BadArg)
    ));

    // `erlang:monitor(process, Port)` is `badarg` in the oracle ...
    let port_term = PortId::new(7).to_eterm();
    assert!(matches!(
        f.table.monitor_term(watcher, port_term, MonitorKind::Process),
        Err(BeamError::BadArg)
    ));
    // ... while `erlang:monitor(port, Port)` is a **typed** gap here: this entry point has no
    // `PortTable`, so it names the faithful entry point instead of fabricating a monitor.
    assert_typed_unsupported(
        f.table
            .monitor_term(watcher, port_term, MonitorKind::Port)
            .expect_err("a port target cannot be monitored without the port table"),
        PORT_NEEDS_PORT_TABLE,
    );

    // `erlang:monitor(port, NotAPort)` is `badarg` — through the typed port entry point too.
    assert!(matches!(
        f.table.monitor_port_term(watcher, make_small(1), &mut ports),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        f.table.monitor_port_term(watcher, tuple, &mut ports),
        Err(BeamError::BadArg)
    ));

    // A **name** target with the port kind needs a port-capable registry; both spellings say so.
    let nobody = f.atom("nobody");
    let node = f.atom("nonode@nohost");
    assert_typed_unsupported(
        f.table
            .monitor_port_term(watcher, make_atom(nobody), &mut ports)
            .expect_err("a named port monitor cannot be resolved"),
        NAMED_PORT_MONITOR,
    );
    assert_typed_unsupported(
        f.table
            .monitor_named(watcher, nobody, node, MonitorKind::Port)
            .expect_err("a named port monitor cannot be resolved"),
        NAMED_PORT_MONITOR,
    );

    // Nothing above touched the table: no monitor half, no allocated reference, no message.
    let watcher_process = f.table.get(watcher).expect("the watcher is live");
    assert!(watcher_process.monitors().is_empty());
    assert!(watcher_process.port_monitors().is_empty());
    assert!(watcher_process.mailbox().is_empty());
}

#[test]
fn a_named_port_monitor_is_never_recorded_as_a_process_monitor() {
    // The wrong-result this pins away: before the fix, `monitor_term(watcher, Name, Port)` and
    // `monitor_named(.., Port)` recorded a **process** monitor on the process the name resolved
    // to (and its DOWN would carry the resolved pid). Now both are the typed `unsupported`.
    let mut f = Fixture::new();
    let name = f.atom("a_registered_process");
    let node = f.atom("nonode@nohost");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let target = f.table.spawn_default().expect("a target process");
    f.table.register(name, target).expect("the name is free");
    f.table.set_local_node(node);

    assert_typed_unsupported(
        f.table
            .monitor_term(watcher, make_atom(name), MonitorKind::Port)
            .expect_err("a named port monitor cannot be resolved"),
        NAMED_PORT_MONITOR,
    );
    // The registered *process* was not monitored and no DOWN was fabricated.
    assert!(f.table.get(watcher).expect("watcher").monitors().is_empty());
    assert!(f.table.get(watcher).expect("watcher").mailbox().is_empty());
    assert!(f
        .table
        .get(target)
        .expect("target")
        .monitored_by()
        .is_empty());

    // The process-kind spelling of the same call is unaffected (the fix is kind-specific).
    let reference = f
        .table
        .monitor_term(watcher, make_atom(name), MonitorKind::Process)
        .expect("a process name still resolves");
    assert_eq!(
        f.table
            .get(watcher)
            .expect("watcher")
            .monitors()
            .find(reference)
            .map(|monitor| monitor.target),
        Some(target)
    );
}

#[test]
fn external_pid_operations_without_a_dist_router_are_record_only() {
    // Pins objective item 8 (the external-pid half): with **no** `DistSignalRouter` the table is
    // record-only. Every relationship change returns `Ok` (there is simply nowhere to send it),
    // the local record is exact, and nothing is fabricated into any local mailbox — the documented
    // behaviour, not a wrong result.
    let mut f = Fixture::new();
    let node = f.atom_term("remote@host");
    let a = f.table.spawn_default().expect("a live process");
    let ext = f.remote_pid(node, 1, 2, 3);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");

    assert!(!f.table.has_dist_router(), "no seam is installed");
    assert!(f.table.take_dist_router().is_none());

    // `link_term` records the remote link (and does not touch the local link set).
    f.table.link_term(a, ext).expect("a is live");
    assert_eq!(f.table.remote_links(a), &[rpid]);
    assert_eq!(
        f.table.get(a).expect("a").links().iter().count(),
        0,
        "an external pid never enters the local link set"
    );

    // `monitor_term` records exactly one `RemoteMonitor` — never a local `Monitor`.
    let reference = f
        .table
        .monitor_term(a, ext, MonitorKind::Process)
        .expect("a is live");
    assert_eq!(
        f.table.remote_monitors(a),
        &[RemoteMonitor {
            reference,
            remote: rpid,
            kind: MonitorKind::Process,
        }]
    );
    let process = f.table.get(a).expect("a is live");
    assert!(
        process.monitors().is_empty(),
        "a remote monitor never enters the local outgoing set"
    );
    assert!(process.port_monitors().is_empty());
    assert!(
        process.mailbox().is_empty(),
        "an unconnected node fabricates no message"
    );

    // `send_exit_term` has nowhere to go; it is a no-op that still reports success.
    f.table
        .send_exit_term(a, ext, make_small(9))
        .expect("a is live");
    assert!(f.table.get(a).expect("a is live").mailbox().is_empty());

    // `unlink_term`/`demonitor` drop the records (the records are the only trace of the change).
    f.table.unlink_term(a, ext).expect("a is live");
    assert!(f.table.remote_links(a).is_empty());
    assert!(f.table.demonitor(a, reference).expect("a is live"));
    assert!(f.table.remote_monitors(a).is_empty());
    assert!(
        !f.table.demonitor(a, reference).expect("a is live"),
        "a second demonitor finds nothing"
    );
}

#[test]
fn unallocated_port_relationship_mutations_are_badarg_without_side_effects() {
    // Pins objective item 8 (the port-table half): every relationship mutation on a port number
    // that is not allocated is `badarg` **and** leaves the port table and the processes untouched.
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let port_atom = f.atom("port");
    let noproc = f.atom_term("noproc");
    let owner = f.table.spawn_default().expect("a port owner");
    let watcher = f.table.spawn_default().expect("a watcher process");

    let mut ports = PortTable::new();
    let live = ports.alloc_owned(owner).expect("room in the number space");
    let reference = RefAllocator::new().next();
    let unknown = PortId::new(9999);
    assert!(!ports.contains(unknown));

    // The `PortTable` relationship surface rejects an unallocated number.
    assert!(matches!(
        ports.monitor(unknown, watcher, reference),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        ports.link(unknown, watcher),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        ports.demonitor(unknown, reference),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(ports.close(unknown), Err(BeamError::BadArg)));
    assert_eq!(
        ports.monitors(live),
        &[] as &[PortMonitor],
        "the live port is untouched"
    );
    assert_eq!(ports.links(live), &[] as &[ProcessId]);

    // The one-call orchestrator rejects it up front: nothing is signalled, nothing changes.
    let before = f.table.len();
    assert!(matches!(
        f.table.port_closed(&mut ports, unknown, make_small(1)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(f.table.len(), before, "no process was terminated");
    assert!(f.table.contains(watcher));
    assert!(f.table.get(watcher).expect("watcher").mailbox().is_empty());
    assert!(ports.contains(live), "the live port is still allocated");

    // `monitor_port` on an unallocated port is deliberately **not** `badarg`: the oracle answers a
    // reference and fires an immediate `noproc` DOWN (the same born-down shape a dead pid gets).
    let born_down = f
        .table
        .monitor_port(watcher, unknown, &mut ports)
        .expect("a port that is already gone is born down");
    assert!(
        f.table
            .get(watcher)
            .expect("watcher")
            .port_monitors()
            .is_empty(),
        "a born-down monitor records no outgoing half"
    );
    let payload = sole_payload(&f.table, watcher);
    // SAFETY: `payload` is the live `{'DOWN', Ref, port, Port, noproc}` tuple the table built on
    // the watcher's own heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(port_atom));
        assert_eq!(tuple_element(payload, 3), unknown.to_eterm());
        assert_eq!(tuple_element(payload, 4), noproc);
    }
    // It is not a recorded monitor, so it is not removable as one (`false`, not an error).
    assert!(!f
        .table
        .demonitor_port(watcher, born_down, &mut ports)
        .expect("the watcher is live"));
}

// ---------------------------------------------------------------------------
// The `monitor(port, Port)` surface (objective item A1)
// ---------------------------------------------------------------------------

#[test]
fn a_live_port_monitor_records_both_halves_and_demonitor_removes_them() {
    // Pins objective item A1: `monitor_port` records the watcher's **outgoing** `PortWatch` and the
    // port's incoming `PortMonitor`; `demonitor_port` resolves both; and an explicit demonitor of a
    // monitor that was never installed reports `false` rather than an error.
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();
    let port = ports.alloc().expect("room in the number space");

    let reference = f
        .table
        .monitor_port(watcher, port, &mut ports)
        .expect("a live port monitors");

    assert_eq!(
        f.table.get(watcher).expect("watcher").port_monitors(),
        &[PortWatch { reference, port }],
        "the outgoing half is recorded on the watcher"
    );
    assert_eq!(
        ports.monitors(port),
        &[PortMonitor { reference, watcher }],
        "the incoming half is recorded on the port"
    );
    assert!(
        f.table.get(watcher).expect("watcher").monitors().is_empty(),
        "a port monitor is not a process monitor"
    );

    // The demonitor resolves both halves and reports that it did.
    assert!(f
        .table
        .demonitor_port(watcher, reference, &mut ports)
        .expect("the watcher is live"));
    assert!(f
        .table
        .get(watcher)
        .expect("watcher")
        .port_monitors()
        .is_empty());
    assert!(ports.monitors(port).is_empty());
    assert!(
        !f.table
            .demonitor_port(watcher, reference, &mut ports)
            .expect("the watcher is live"),
        "a second demonitor_port finds nothing"
    );

    // An unknown watcher is the table's ordinary `TermNotFound`.
    assert!(matches!(
        f.table
            .demonitor_port(ProcessId::new(999, 0), reference, &mut ports),
        Err(BeamError::TermNotFound)
    ));
    assert!(matches!(
        f.table
            .monitor_port(ProcessId::new(999, 0), port, &mut ports),
        Err(BeamError::TermNotFound)
    ));
    // ... and the unknown-watcher calls left the live port's relationship set alone.
    assert!(ports.monitors(port).is_empty());
}

#[test]
fn port_closed_resolves_both_halves_and_delivers_the_port_tagged_down() {
    // Pins objective item A1's orchestrator: closing a monitored port delivers the oracle's
    // `{'DOWN', Ref, port, Port, Reason}` **and** resolves the watcher's outgoing half, so
    // `process_info(Watcher, monitors)` would no longer list it.
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let port_atom = f.atom("port");
    let watcher = f.table.spawn_default().expect("a watcher process");

    let mut ports = PortTable::new();
    let port = ports.alloc().expect("room in the number space");
    let reference = f
        .table
        .monitor_port(watcher, port, &mut ports)
        .expect("a live port monitors");

    let reason = f.atom_term("normal");
    f.table
        .port_closed(&mut ports, port, reason)
        .expect("the port is live");

    assert!(!ports.contains(port), "the port number is freed");
    assert!(
        f.table
            .get(watcher)
            .expect("watcher")
            .port_monitors()
            .is_empty(),
        "the port's death resolved the outgoing half"
    );
    let payload = sole_payload(&f.table, watcher);
    // SAFETY: `payload` is the live `{'DOWN', Ref, port, Port, Reason}` tuple the table built on
    // the watcher's own heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(port_atom));
        assert_eq!(tuple_element(payload, 3), port.to_eterm());
        assert_eq!(tuple_element(payload, 4), reason);
    }
    // The resolved monitor is not removable as a live one any more.
    assert!(!f
        .table
        .demonitor_port(watcher, reference, &mut ports)
        .expect("the watcher is live"));
}

#[test]
fn the_outgoing_port_watch_set_is_not_a_collector_root() {
    // Pins the audit fact that the outgoing half adds **no** GC root: a `PortWatch` is a reference
    // plus a port number (plain values), so a safepoint collection neither relocates nor needs it.
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();
    let port = ports.alloc().expect("room in the number space");
    let reference = f
        .table
        .monitor_port(watcher, port, &mut ports)
        .expect("a live port monitors");

    // A heap term that the collection must keep reachable through the register roots.
    let tuple = f.tuple_in(watcher, &[make_small(11), make_small(22)]);
    {
        let process = f.table.get_mut(watcher).expect("watcher");
        // A register must be live before it can be written.
        process.registers_mut().x_mut().set_live(1).expect("in range");
        process.registers_mut().x_mut().set(0, tuple).expect("in range");
        assert_eq!(process.registers().x().live(), 1);
    }

    f.table
        .get_mut(watcher)
        .expect("watcher")
        .collect()
        .expect("the collection succeeds");

    // The out-of-heap port watch is untouched by the collection ...
    assert_eq!(
        f.table.get(watcher).expect("watcher").port_monitors(),
        &[PortWatch { reference, port }],
        "a port watch is not a term and is never gathered/scattered"
    );
    // ... the incoming half is still on the port ...
    assert_eq!(ports.monitors(port), &[PortMonitor { reference, watcher }]);
    // ... and the register root survived (the collection still relocated real heap terms).
    let relocated = f
        .table
        .get(watcher)
        .expect("watcher")
        .registers()
        .x()
        .get(0)
        .expect("x(0) is in range");
    // SAFETY: `relocated` is the live 2-tuple the table built on the watcher's heap (and just
    // relocated through the register root); both element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(relocated), 2);
        assert_eq!(tuple_element(relocated, 0), make_small(11));
        assert_eq!(tuple_element(relocated, 1), make_small(22));
    }

    // And the monitor is still fully functional after the collection.
    f.table
        .port_closed(&mut ports, port, make_small(1))
        .expect("the port is live");
    assert!(f
        .table
        .get(watcher)
        .expect("watcher")
        .port_monitors()
        .is_empty());
    let down = f.atom("DOWN");
    assert!(payload_is_down_for(
        sole_payload(&f.table, watcher),
        reference,
        down
    ));
}

// ---------------------------------------------------------------------------
// Oracle cross-checks (the real system ERTS)
// ---------------------------------------------------------------------------

/// `monitor/2`'s type dispatch: `time_offset`, an unknown type atom, and bad targets are `badarg`.
#[test]
fn oracle_monitor_type_dispatch_and_bad_targets_are_badarg() {
    let expr = r#"
        F = fun(E) -> try E() of _ -> ok catch error:badarg -> badarg end end,
        R1 = F(fun() -> erlang:monitor(time_offset, self()) end),
        R2 = F(fun() -> erlang:monitor(bogus_type, self()) end),
        R3 = F(fun() -> erlang:monitor(port, 42) end),
        R4 = F(fun() -> erlang:monitor(port, {a, b}) end),
        R5 = F(fun() -> erlang:monitor(process, 42) end),
        R6 = F(fun() -> erlang:monitor(port, make_ref()) end),
        io:format("~p|~p|~p|~p|~p|~p~n", [R1, R2, R3, R4, R5, R6])
    "#;
    if !expect_oracle(expr, "badarg|badarg|badarg|badarg|badarg|badarg") {
        return;
    }

    // The Rust side of the *type* half of the same statement (the target half is pinned above).
    assert_eq!(MonitorKind::from_type_name("time_offset"), None);
    assert_eq!(MonitorKind::from_type_name("bogus_type"), None);
}

/// A `monitor(port, Port)` on a **closed** port is born down with `noproc` (not `badarg`).
#[test]
fn oracle_monitor_of_a_closed_port_is_born_down_with_noproc() {
    // A **closed** port with no OS child: `list_to_port/1` on a number no live port owns yields a
    // port term the port table does not know, which is exactly the "gone" case this check pins.
    // (An `open_port({spawn, "sleep N"}, [])` probe would work too, but the spawned child inherits
    // `erl`'s stderr — which `Command::output()` captures through a pipe — so the driver would
    // block until the child died. Keeping the oracle probes child-free keeps them fast.)
    let expr = r##"
        P = list_to_port("#Port<0.9999>"),
        undefined = erlang:port_info(P),
        M = erlang:monitor(port, P),
        {Tag, Reason} = receive {'DOWN', M, T, P, R} -> {T, R} after 1000 -> timeout end,
        io:format("~p|~p~n", [Tag, Reason])
    "##;
    if !expect_oracle(expr, "port|noproc") {
        return;
    }

    // The Rust side: an unallocated port is born down with the same tag/reason/target shape.
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let port_atom = f.atom("port");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();
    let closed = PortId::new(4242);
    let reference = f
        .table
        .monitor_port(watcher, closed, &mut ports)
        .expect("a port that is gone is born down");
    let payload = sole_payload(&f.table, watcher);
    assert!(payload_is_down_for(payload, reference, down));
    // SAFETY: `payload` is the live DOWN tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_element(payload, 2), make_atom(port_atom));
        assert_eq!(tuple_element(payload, 3), closed.to_eterm());
        assert_eq!(tuple_element(payload, 4), noproc);
    }
}

/// A live port monitor is visible in `process_info(self(), monitors)` as `{port, Port}`.
#[test]
fn oracle_a_live_port_monitor_appears_in_process_info_monitors() {
    // A **live** port with no OS child to leak: the runtime always owns at least one port
    // (`erl_prim_loader`'s), so the head of `erlang:ports()` is a genuine live port. (A `spawn`ed
    // port program would inherit `erl`'s stderr — captured through a pipe by `Command::output()` —
    // and stall the driver until the child died; keeping the probes child-free keeps them fast.)
    let expr = r#"
        [P | _] = erlang:ports(),
        M = erlang:monitor(port, P),
        {monitors, L} = process_info(self(), monitors),
        [{port, _}] = L,
        D = erlang:demonitor(M),
        {monitors, L2} = process_info(self(), monitors),
        io:format("~p|~p|~p~n", [D, length(L), length(L2)])
    "#;
    if !expect_oracle(expr, "true|1|0") {
        return;
    }

    // The Rust side: the outgoing half appears in the port-watch set and `demonitor_port`
    // removes it.
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();
    let port = ports.alloc().expect("room in the number space");
    let reference = f
        .table
        .monitor_port(watcher, port, &mut ports)
        .expect("a live port monitors");
    assert_eq!(
        f.table.get(watcher).expect("watcher").port_monitors(),
        &[PortWatch { reference, port }],
        "one entry, exactly as the oracle's single port-tagged tuple"
    );
    assert!(f
        .table
        .demonitor_port(watcher, reference, &mut ports)
        .expect("the watcher is live"));
    assert!(f
        .table
        .get(watcher)
        .expect("watcher")
        .port_monitors()
        .is_empty());
}
