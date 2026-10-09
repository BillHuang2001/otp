//! Integration evidence for **cleaning up a dead port-watcher's monitor**.
//!
//! When a process that monitors a port (`erlang:monitor(port, Port)`) dies, the port must stop
//! listing it: `erlang:port_info(Port, monitored_by)` no longer contains the dead pid. This crate
//! keeps the two halves of a port monitor in two places — the outgoing [`PortWatch`] on the
//! watcher and the incoming [`PortMonitor`] in the [`PortTable`] — and the process finalizer
//! drains the outgoing half and removes the matching incoming entry, but only when it is handed
//! the port table: [`ProcessTable::terminate_with_ports`] (and its schedule-in sibling
//! [`ProcessTable::service_signals_with_ports`]) do, plain [`ProcessTable::terminate`] and
//! [`ProcessTable::service_signals`] cannot (no [`PortTable`] handle) and leave the port side
//! untouched — both behaviours are pinned below.
//!
//! [`oracle_a_dead_port_watcher_leaves_port_info_monitored_by`] is the cross-check against the
//! real system Erlang (OTP 29.1.1 / erts-17.1), the crate-local slice of the workspace
//! *compatibility gate*: it drives `erl` with a tiny `-noshell -eval` probe (a process monitors a
//! port, dies, and `erlang:port_info(Port, monitored_by)` is observed before and after) and
//! asserts the oracle's own answer. `erl`'s `stdout`/`stderr` are captured (never inherited) and
//! the probe runs on a thread with a timeout, so a host without a functioning `erl` **skips
//! loudly** and never hangs the suite.

mod common;

use beam_process::prelude::*;
use beam_term::prelude::{make_small, AtomTable};

use common::erl_stdout;

/// A table plus the atom intern table behind its signal atoms, and a port table.
struct Fixture {
    table: ProcessTable,
    ports: PortTable,
}

impl Fixture {
    fn new() -> Fixture {
        let atoms = SignalAtoms::from_table(&mut AtomTable::new());
        Fixture {
            table: ProcessTable::new(atoms),
            ports: PortTable::new(),
        }
    }

    /// A fresh process owned by the fixture's table.
    fn spawn(&mut self) -> ProcessId {
        self.table.spawn_default().expect("room in the pid table")
    }

    /// A fresh port owned by `owner`.
    fn port(&mut self, owner: ProcessId) -> PortId {
        self.ports
            .alloc_owned(owner)
            .expect("room in the port number space")
    }
}

/// The single incoming monitor's reference on `port`, panicking unless exactly one is present.
fn sole_reference(ports: &PortTable, port: PortId) -> MonitorRef {
    let monitors = ports.monitors(port);
    assert_eq!(monitors.len(), 1, "exactly one incoming monitor");
    monitors[0].reference
}

#[test]
fn a_dying_port_watcher_loses_its_incoming_monitor() {
    let mut fx = Fixture::new();
    let owner = fx.spawn();
    let watcher = fx.spawn();
    let port = fx.port(owner);
    let reference = fx
        .table
        .monitor_port(watcher, port, &mut fx.ports)
        .expect("the watcher is live and the port is allocated");
    assert_eq!(sole_reference(&fx.ports, port), reference);
    assert_eq!(
        fx.table
            .get(watcher)
            .expect("watcher")
            .port_monitors()
            .len(),
        1,
        "the outgoing half is recorded on the watcher"
    );

    fx.table
        .terminate_with_ports(&mut fx.ports, watcher, make_small(1))
        .expect("the watcher is live");

    assert!(!fx.table.contains(watcher), "the watcher died");
    assert!(
        fx.ports.monitors(port).is_empty(),
        "the dead watcher's incoming monitor is gone (`port_info(Port, monitored_by)` stops \
         listing it) — the outgoing half was drained and used to find this entry"
    );
    assert!(
        fx.ports.contains(port),
        "the port itself is untouched by the watcher's death"
    );
}

#[test]
fn terminate_without_ports_leaves_the_port_side_untouched() {
    // The documented delegation: plain `terminate` has no `PortTable`, so it cannot remove the
    // incoming entry. A runtime holding the port table must use `terminate_with_ports`.
    let mut fx = Fixture::new();
    let owner = fx.spawn();
    let watcher = fx.spawn();
    let port = fx.port(owner);
    let reference = fx
        .table
        .monitor_port(watcher, port, &mut fx.ports)
        .expect("the watcher is live and the port is allocated");

    fx.table.terminate(watcher, make_small(1)).expect("the watcher is live");

    assert!(!fx.table.contains(watcher));
    assert_eq!(
        sole_reference(&fx.ports, port),
        reference,
        "without the port table the incoming entry cannot be removed"
    );
}

#[test]
fn a_link_cascade_cleans_the_dying_peers_port_monitors() {
    let mut fx = Fixture::new();
    let a = fx.spawn();
    let b = fx.spawn();
    let owner = fx.spawn();
    fx.table.link(a, b).expect("both pids are live");

    let port = fx.port(owner);
    let reference = fx
        .table
        .monitor_port(b, port, &mut fx.ports)
        .expect("b is live and the port is allocated");
    assert_eq!(sole_reference(&fx.ports, port), reference);

    // A dies; the non-trapping linked peer B follows, and B's own port monitor must go too.
    fx.table
        .terminate_with_ports(&mut fx.ports, a, make_small(9))
        .expect("a is live");

    assert!(!fx.table.contains(a) && !fx.table.contains(b), "both died");
    assert!(
        fx.ports.monitors(port).is_empty(),
        "the cascade cleaned B's incoming port monitor"
    );
}

#[test]
fn a_schedule_in_cascade_with_ports_cleans_port_monitors() {
    let mut fx = Fixture::new();
    let target = fx.spawn();
    let sender = fx.spawn();
    let owner = fx.spawn();

    let port = fx.port(owner);
    let reference = fx
        .table
        .monitor_port(target, port, &mut fx.ports)
        .expect("target is live and the port is allocated");
    assert_eq!(sole_reference(&fx.ports, port), reference);

    // The single scheduler marks the executing process `Running`; its signal is deferred.
    fx.table
        .get_mut(target)
        .expect("target")
        .set_state(ProcessState::Running);
    fx.table
        .send_exit(sender, target, make_small(7))
        .expect("a live target");
    assert!(fx.table.contains(target), "the signal is deferred");

    // The schedule-in servicing (ports-aware) applies it: the target dies and its port monitor
    // is cleaned.
    fx.table
        .service_signals_with_ports(&mut fx.ports, target)
        .expect("a live pid");
    assert!(!fx.table.contains(target), "the deferred signal terminated it");
    assert!(
        fx.ports.monitors(port).is_empty(),
        "the schedule-in cascade cleaned the port monitor"
    );
}

#[test]
fn a_schedule_in_cascade_without_ports_leaves_the_port_side() {
    // The `None`-ports delegation of `service_signals` cannot clean the port side.
    let mut fx = Fixture::new();
    let target = fx.spawn();
    let sender = fx.spawn();
    let owner = fx.spawn();

    let port = fx.port(owner);
    let reference = fx
        .table
        .monitor_port(target, port, &mut fx.ports)
        .expect("target is live and the port is allocated");

    fx.table
        .get_mut(target)
        .expect("target")
        .set_state(ProcessState::Running);
    fx.table
        .send_exit(sender, target, make_small(7))
        .expect("a live target");

    fx.table.service_signals(target).expect("a live pid");
    assert!(!fx.table.contains(target));
    assert_eq!(
        sole_reference(&fx.ports, port),
        reference,
        "without the port table the incoming entry remains"
    );
}

#[test]
fn port_closed_cleans_a_dying_linked_peers_other_port_monitors() {
    // `port_closed` already holds the port table; a peer it terminates must clean the peer's
    // *other* port monitors too (the ports-aware cascade).
    let mut fx = Fixture::new();
    let owner = fx.spawn();
    let peer = fx.spawn();

    let closing = fx.port(owner);
    fx.ports.link(closing, peer).expect("the port is allocated");
    let watched = fx.port(owner);
    let reference = fx
        .table
        .monitor_port(peer, watched, &mut fx.ports)
        .expect("peer is live and the watched port is allocated");
    assert_eq!(sole_reference(&fx.ports, watched), reference);

    // The closing port's exit terminates the non-trapping linked peer.
    fx.table
        .port_closed(&mut fx.ports, closing, make_small(5))
        .expect("the port is allocated");

    assert!(!fx.table.contains(peer), "the linked peer terminated");
    assert!(
        fx.ports.monitors(watched).is_empty(),
        "the peer's monitor on the other port was cleaned by the cascade"
    );
}

// ---------------------------------------------------------------------------
// Oracle cross-check (the real system Erlang)
// ---------------------------------------------------------------------------

// The oracle driver (`erl_stdout` and its stdout normalisation) is shared with the crate's other
// `oracle_*` files; it lives in `tests/common/mod.rs` so the logger-suppression fix exists once.
// `erl` runs with `-kernel logger_level none` (and its stdout is normalised), so a crashed probe
// helper's run-time error report can never make these comparisons flaky.

/// The `erl` probe: a process monitors a long-lived port, **is observed to be listed** by
/// `erlang:port_info(Port, monitored_by)`, is then stopped, and is observed to be **gone** from
/// that list. Both observations poll with a bounded budget (the port records the monitor
/// asynchronously), and both results are printed as `ok ok` — the non-vacuous proof is that the
/// `before` poll had to *wait* for the entry to appear.
///
/// The command is `exec sleep 30 >/dev/null 2>&1` so the OS child does **not** inherit (and hold
/// open) the probe's own stdout/stderr pipes: otherwise the reader of those pipes would block
/// until the child exits, even though `erl` itself halts immediately. (`{spawn, Cmd}` runs `Cmd`
/// through `/bin/sh -c`, so the redirection applies before the `exec`.)
const ORACLE_PORT_WATCH_PROBE: &str = r#"
Port = open_port({spawn, "exec sleep 30 >/dev/null 2>&1"}, []),
Self = self(),
Pid = spawn(fun() -> _ = erlang:monitor(port, Port), Self ! registered,
                      receive stop -> ok end end),
receive registered -> ok end,
Wait = fun W(N, Want) ->
    case length(element(2, erlang:port_info(Port, monitored_by))) of
        Want -> ok;
        _ when N =< 0 -> timeout;
        _ -> timer:sleep(5), W(N - 1, Want)
    end
end,
Before = Wait(400, 1),
M = erlang:monitor(process, Pid),
Pid ! stop,
receive {'DOWN', M, process, Pid, _} -> ok end,
After = Wait(400, 0),
io:format("~p ~p~n", [Before, After])
"#;

#[test]
fn oracle_a_dead_port_watcher_leaves_port_info_monitored_by() {
    let Some(out) = erl_stdout(ORACLE_PORT_WATCH_PROBE) else {
        return;
    };
    assert_eq!(
        out.trim(),
        "ok ok",
        "the oracle must list the live port watcher and stop listing it once it dies (a \
         `timeout` means the port never recorded/never released the monitor)"
    );
}

/// The same end state the Rust test asserts, expressed in the oracle's own terms: the two
/// observations above are `{monitored_by, [Pid]}` (non-empty) and `{monitored_by, []}` (empty).
#[test]
fn oracle_port_info_monitored_by_shapes_match_the_rust_pin() {
    // A dedicated probe that prints the two *shapes*, so the oracle's answer is pinned directly
    // rather than only through the boolean probe above. It reuses the same protocol.
    let expr = r#"
Port = open_port({spawn, "exec sleep 30 >/dev/null 2>&1"}, []),
Self = self(),
Pid = spawn(fun() -> _ = erlang:monitor(port, Port), Self ! registered,
                      receive stop -> ok end end),
receive registered -> ok end,
Wait = fun W(N, Want) ->
    case length(element(2, erlang:port_info(Port, monitored_by))) of
        Want -> ok;
        _ when N =< 0 -> timeout;
        _ -> timer:sleep(5), W(N - 1, Want)
    end
end,
ok = Wait(400, 1),
{monitored_by, Before} = erlang:port_info(Port, monitored_by),
M = erlang:monitor(process, Pid),
Pid ! stop,
receive {'DOWN', M, process, Pid, _} -> ok end,
ok = Wait(400, 0),
{monitored_by, After} = erlang:port_info(Port, monitored_by),
io:format("~p ~p~n", [Before =:= [Pid], After])
"#;
    let Some(out) = erl_stdout(expr) else {
        return;
    };
    assert_eq!(
        out.trim(),
        "true []",
        "while alive the port lists exactly the watcher; once it dies the list is empty"
    );
}