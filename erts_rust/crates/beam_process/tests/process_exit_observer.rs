//! Integration evidence for the **process-death observer seam**
//! ([`ProcessExitObserver`]/[`ProcessExit`]).
//!
//! Pins the contract stated in `src/exit_observer.rs`: a registered observer is invoked
//! **exactly once** per death, from the crate's sole finalizer
//! ([`ProcessTable::terminate`]/[`ProcessTable::terminate_with_ports`]), with the dead process's
//! `pid`, exit `reason`, registered `name` and drained outgoing `port_monitors`, **after** the
//! death has been propagated to links/monitors and **before** the dying heap is released. A
//! link cascade notifies once per death; with no observer installed termination is unchanged;
//! and a **panicking** observer cannot corrupt the table (the panic is contained, the pid is
//! still reclaimed, and the table keeps spawning/delivering with consistent relationships).
//!
//! The recorder copies only **immediate** reasons (small integers, atoms) and the
//! heap-independent `name`/`port_monitors` fields — reading a heap-resident `reason` after the
//! dying heap was released would dangle, which is exactly the contract `ProcessExit::reason`
//! documents.
//!
//! The `erl`-cross-checked evidence for the port-monitor cleanup lives in
//! `tests/port_monitor_cleanup.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use beam_process::prelude::*;
use beam_term::prelude::{is_atom, make_small, Atom, AtomTable, Eterm};

/// One recorded death.
#[derive(Clone, Debug)]
struct Event {
    pid: ProcessId,
    reason: Eterm,
    name: Option<Atom>,
    port_monitors: Vec<PortWatch>,
}

/// A shared event log an observer and the test both hold.
type Log = Rc<RefCell<Vec<Event>>>;

/// An observer that records each death, copying only heap-independent payload fields.
struct Recorder {
    log: Log,
}

impl ProcessExitObserver for Recorder {
    fn process_exited(&mut self, exit: &ProcessExit<'_>) {
        self.log.borrow_mut().push(Event {
            pid: exit.pid(),
            reason: exit.reason(),
            name: exit.name(),
            port_monitors: exit.port_monitors().to_vec(),
        });
    }
}

/// An observer that always panics — to prove the panic is contained.
struct Panicker;

impl ProcessExitObserver for Panicker {
    fn process_exited(&mut self, _exit: &ProcessExit<'_>) {
        panic!("observer boom");
    }
}

/// A table plus the atom intern table behind its signal atoms.
struct Fixture {
    table: ProcessTable,
    names: AtomTable,
    normal: Eterm,
    kill: Eterm,
}

impl Fixture {
    fn new() -> Fixture {
        let mut names = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut names);
        let normal = atoms.normal_term();
        let kill = atoms.kill_term();
        Fixture {
            table: ProcessTable::new(atoms),
            names,
            normal,
            kill,
        }
    }

    /// Installs a recording observer, returning the shared log.
    fn record(&mut self) -> Log {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        self.table
            .set_process_exit_observer(Box::new(Recorder { log: log.clone() }));
        log
    }

    fn intern(&mut self, name: &str) -> Atom {
        self.names.intern(name)
    }
}

/// The payload of `pid`'s single queued message (panics unless exactly one is queued).
fn sole_payload(table: &ProcessTable, pid: ProcessId) -> Eterm {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued").payload()
}

#[test]
fn an_observer_is_installed_reported_and_taken() {
    let mut fx = Fixture::new();
    assert!(
        !fx.table.has_process_exit_observer(),
        "the table starts with no observer"
    );
    let log = fx.record();
    assert!(fx.table.has_process_exit_observer());
    assert!(log.borrow().is_empty());

    // Taking the observer out hands it back and leaves the table with none.
    assert!(
        fx.table.take_process_exit_observer().is_some(),
        "the installed observer is returned"
    );
    assert!(!fx.table.has_process_exit_observer());
    assert!(
        fx.table.take_process_exit_observer().is_none(),
        "taking twice yields nothing"
    );
}

#[test]
fn a_normal_exit_notifies_once_with_the_pid_and_reason() {
    let mut fx = Fixture::new();
    let log = fx.record();
    let a = fx.table.spawn_default().expect("a");

    fx.table.terminate(a, fx.normal).expect("a is live");

    let events = log.borrow();
    assert_eq!(events.len(), 1, "exactly one death, exactly one call");
    assert_eq!(events[0].pid, a);
    assert!(is_atom(events[0].reason), "the reason travelled verbatim");
    assert_eq!(events[0].reason, fx.normal);
    assert_eq!(events[0].name, None, "an unregistered process has no name");
    assert!(events[0].port_monitors.is_empty());
}

#[test]
fn a_killed_process_notifies_with_the_kill_reason() {
    let mut fx = Fixture::new();
    let log = fx.record();
    let victim = fx.table.spawn_default().expect("victim");

    fx.table.terminate(victim, fx.kill).expect("victim is live");

    let events = log.borrow();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pid, victim);
    assert_eq!(events[0].reason, fx.kill);
}

#[test]
fn the_payload_reports_the_registered_name() {
    let mut fx = Fixture::new();
    let name = fx.intern("observer_registered");
    let log = fx.record();
    let pid = fx.table.spawn_default().expect("pid");
    fx.table.register(name, pid).expect("the name is free");

    fx.table.terminate(pid, make_small(1)).expect("pid is live");

    let events = log.borrow();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].name,
        Some(name),
        "the name is captured before the registry entry is dropped"
    );
    drop(events);
    assert_eq!(fx.table.whereis(name), None, "the registration is gone");
}

#[test]
fn a_link_cascade_notifies_once_per_death() {
    let mut fx = Fixture::new();
    let log = fx.record();
    let a = fx.table.spawn_default().expect("a");
    let b = fx.table.spawn_default().expect("b");
    fx.table.link(a, b).expect("both pids are live");

    // A dies, B is a non-trapping linked peer and follows with the same reason.
    let reason = make_small(9);
    fx.table.terminate(a, reason).expect("a is live");

    assert!(!fx.table.contains(a) && !fx.table.contains(b), "both died");
    let events = log.borrow();
    assert_eq!(events.len(), 2, "one call per death, not one per termination");
    // A is notified from `terminate_into` before the cascade services B.
    assert_eq!(events[0].pid, a);
    assert_eq!(events[0].reason, reason);
    assert_eq!(events[1].pid, b);
    assert_eq!(events[1].reason, reason);
}

#[test]
fn the_payload_reports_the_drained_port_monitors() {
    let mut fx = Fixture::new();
    let log = fx.record();
    let owner = fx.table.spawn_default().expect("owner");
    let watcher = fx.table.spawn_default().expect("watcher");

    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner).expect("room in the number space");
    let reference = fx
        .table
        .monitor_port(watcher, port, &mut ports)
        .expect("the watcher is live and the port is allocated");

    fx.table
        .terminate_with_ports(&mut ports, watcher, make_small(3))
        .expect("the watcher is live");

    let events = log.borrow();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pid, watcher);
    assert_eq!(
        events[0].port_monitors,
        vec![PortWatch { reference, port }],
        "the drained outgoing watch is shown to the observer"
    );
    drop(events);
    assert!(
        ports.monitors(port).is_empty(),
        "and the incoming entry was removed"
    );
}

#[test]
fn a_live_process_with_no_signals_notifies_nothing() {
    let mut fx = Fixture::new();
    let log = fx.record();
    let pid = fx.table.spawn_default().expect("pid");

    fx.table.service_signals(pid).expect("a live pid is a no-op");
    assert!(log.borrow().is_empty(), "no death, no notification");
    assert!(fx.table.contains(pid));
}

#[test]
fn without_an_observer_terminate_is_unchanged() {
    let mut fx = Fixture::new();
    let target = fx.table.spawn_default().expect("target");
    let watcher = fx.table.spawn_default().expect("watcher");
    fx.table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("the watcher is live");

    assert!(!fx.table.has_process_exit_observer());
    fx.table.terminate(target, make_small(5)).expect("target is live");

    // The observable end state is exactly today's: the pid is reclaimed, the monitor's DOWN is
    // delivered, and the table keeps working.
    assert!(!fx.table.contains(target));
    let _ = sole_payload(&fx.table, watcher);
    let fresh = fx.table.spawn_default().expect("the table still spawns");
    fx.table
        .deliver_simple(fresh, make_small(0), make_small(42))
        .expect("the table still delivers");
    assert_eq!(
        fx.table.get(fresh).expect("fresh").mailbox().len(),
        1,
        "delivery still reaches the recipient"
    );
}

#[test]
fn a_panicking_observer_cannot_corrupt_the_table() {
    let mut fx = Fixture::new();
    fx.table.set_process_exit_observer(Box::new(Panicker));

    let watcher = fx.table.spawn_default().expect("watcher");
    let peer = fx.table.spawn_default().expect("trapping linked peer");
    fx.table.get_mut(peer).expect("peer").set_trap_exit(true);
    let target = fx.table.spawn_default().expect("target");
    fx.table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("the watcher is live");
    fx.table.link(peer, target).expect("both pids are live");

    // The panic is caught, so termination returns normally.
    fx.table
        .terminate(target, make_small(7))
        .expect("the panic did not escape terminate");

    // The table is fully consistent: the pid is reclaimed, the link is gone, both the DOWN and
    // the trapped EXIT were delivered, and spawning/delivering still work.
    assert!(!fx.table.contains(target), "the pid is still reclaimed");
    assert!(
        fx.table
            .get(peer)
            .expect("peer")
            .links()
            .iter()
            .all(|p| p != target),
        "the dying process's back-link was dropped"
    );
    let _ = sole_payload(&fx.table, watcher);
    let _ = sole_payload(&fx.table, peer);

    let fresh = fx.table.spawn_default().expect("the table still spawns");
    fx.table
        .deliver_simple(fresh, make_small(0), make_small(1))
        .expect("the table still delivers");
    assert_eq!(fx.table.get(fresh).expect("fresh").mailbox().len(), 1);

    // The panicking observer is still installed (a panic is not an uninstall).
    assert!(fx.table.has_process_exit_observer());
}

#[test]
fn a_panicking_observer_survives_a_link_cascade() {
    let mut fx = Fixture::new();
    fx.table.set_process_exit_observer(Box::new(Panicker));
    let a = fx.table.spawn_default().expect("a");
    let b = fx.table.spawn_default().expect("b");
    fx.table.link(a, b).expect("both pids are live");

    // The cascade terminates two processes; the observer panics for each, both are contained.
    fx.table.terminate(a, make_small(11)).expect("a is live");

    assert!(!fx.table.contains(a) && !fx.table.contains(b));
    let fresh = fx.table.spawn_default().expect("the table still spawns");
    assert!(fx.table.contains(fresh));
}
