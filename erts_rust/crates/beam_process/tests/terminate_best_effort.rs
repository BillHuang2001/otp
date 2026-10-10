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

//! Integration evidence for the **best-effort termination cascade** of
//! [`ProcessTable::terminate`]/[`ProcessTable::terminate_with_ports`].
//!
//! A dying process is reclaimed **first** (`take` bumps its slot serial), so by the time its
//! death is propagated the death has *already happened*. Every per-peer step of the cascade is
//! therefore best-effort: a peer that cannot be signalled — a local heap that cannot take the
//! reason copy, a remote node whose transport is down — is **dropped and logged** (exactly ERTS's
//! `erts_dsig_drop` for a control signal it cannot deliver) and the cascade **continues**, so the
//! process-death observer always fires and the port table's dangling incoming `PortMonitor`
//! entries are always removed.
//!
//! Aborting mid-cascade was the old behaviour: one unreachable peer left every *later* peer
//! unsignalled, the observer unrun (leaking the host resources it releases — the ETS tables owned
//! by the pid are the motivating case) and the port table holding a dead watcher's monitor.
//!
//! Two failures are forced, each hitting a different step:
//!
//! 1. [`a_failing_dist_router_does_not_abort_the_cascade`] — the **remote** step (4): a
//!    [`DistSignalRouter`] whose transport always fails. The router is attempted once per remote
//!    link partner, the local watcher still gets its `{'DOWN', …}`, the observer still fires and
//!    the dead watcher's port monitor is still cleaned.
//! 2. [`an_unreachable_peer_heap_does_not_abort_the_cascade`] — the **local link** step (1): a
//!    relocation-exempt peer whose bounded nursery cannot hold the exit reason. The *next* link
//!    partner is still signalled, the observer still fires and the port monitor is still cleaned.
//!
//! Determinism: no sleep, no wall-clock bound and no oracle. The oversized reason is derived from
//! the public heap-policy arithmetic (the peer's own bounded nursery capacity + 1 word), so the
//! refusal is exact — the same construction `tests/signal_exhaustion.rs` uses.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_process::prelude::*;
use beam_term::prelude::{
    external_pid_size_words, make_atom, make_small, size_object, write_external_pid, AtomTable,
    Eterm,
};

/// A distribution seam whose transport **always** fails, recording every attempt.
///
/// This is the "peer that cannot be reached" the best-effort contract is about: a down remote
/// node, or a transport I/O error. The log is shared through an `Rc<RefCell<_>>` because the seam
/// lives behind a `Box<dyn DistSignalRouter>` inside the table and is not downcastable — the test
/// keeps its own handle to inspect what was attempted.
struct FailingRouter {
    attempts: Rc<RefCell<Vec<DistSignal>>>,
}

impl DistSignalRouter for FailingRouter {
    fn route(&mut self, signal: DistSignal) -> Result<()> {
        self.attempts.borrow_mut().push(signal);
        Err(BeamError::Io(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "remote node unreachable",
        )))
    }
}

/// A process-death observer that records the pid of every death.
struct Recorder {
    deaths: Rc<RefCell<Vec<ProcessId>>>,
}

impl ProcessExitObserver for Recorder {
    fn process_exited(&mut self, exit: &ProcessExit<'_>) {
        self.deaths.borrow_mut().push(exit.pid());
    }
}

/// A table, a port table, and the remote sysname atom.
struct Fixture {
    table: ProcessTable,
    ports: PortTable,
    node: Eterm,
}

impl Fixture {
    fn new() -> Fixture {
        let mut names = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut names);
        let node = make_atom(names.intern("remote@host"));
        Fixture {
            table: ProcessTable::new(atoms),
            ports: PortTable::new(),
            node,
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
            .expect("room in the port-number space")
    }

    /// Installs a recording observer, returning the shared death log.
    fn record(&mut self) -> Rc<RefCell<Vec<ProcessId>>> {
        let deaths = Rc::new(RefCell::new(Vec::new()));
        self.table.set_process_exit_observer(Box::new(Recorder {
            deaths: Rc::clone(&deaths),
        }));
        deaths
    }
}

/// Builds an external pid term on `heap` (the frozen 4-word `[header, node, creation, data]`
/// thing).
fn remote_pid(heap: &mut Heap, node: Eterm, number: u32, serial: u32, creation: u32) -> Eterm {
    // SAFETY: `heap` hands out `external_pid_size_words()` (4) exclusively-owned, writable words.
    let p = unsafe {
        heap.alloc_words(external_pid_size_words())
            .expect("the heap has room")
    };
    // SAFETY: `p` addresses the four words just handed out and `write_external_pid` writes all of
    // them before the pointer escapes; `node` is a live atom term.
    unsafe { write_external_pid(p, node, number, serial, creation) }
}

/// The number of messages in `pid`'s mailbox.
fn mailbox_len(table: &ProcessTable, pid: ProcessId) -> usize {
    table
        .get(pid)
        .expect("the recipient is live")
        .mailbox()
        .len()
}

/// A down distribution seam must not abort a death: every remote link partner is attempted (and
/// its failure logged), and the steps *after* the remote step — the death observer and the port
/// monitor cleanup — still run.
#[test]
fn a_failing_dist_router_does_not_abort_the_cascade() {
    let mut fx = Fixture::new();
    let mut heap = Heap::new(64).expect("test heap");
    let attempts = Rc::new(RefCell::new(Vec::new()));
    fx.table.set_dist_router(Box::new(FailingRouter {
        attempts: Rc::clone(&attempts),
    }));

    let dying = fx.spawn();
    let watcher = fx.spawn();
    let owner = fx.spawn();

    // Step 2: a local watcher — a step *before* the failing remote step, and the peer whose
    // `{'DOWN', …}` proves the cascade reached step 2.
    fx.table
        .monitor(watcher, dying, MonitorKind::Process)
        .expect("the watcher is live");
    // Step 6: an incoming port monitor, so the cleanup after the failing step is observable.
    let port = fx.port(owner);
    fx.table
        .monitor_port(dying, port, &mut fx.ports)
        .expect("the dying process is live and the port is allocated");
    // Step 5: the process-death observer (host-owned resources are released from here).
    let deaths = fx.record();
    // Step 4: two remote link partners.
    let ext1 = remote_pid(&mut heap, fx.node, 11, 22, 33);
    let ext2 = remote_pid(&mut heap, fx.node, 44, 55, 66);
    let rpid1 = RemotePid::from_eterm(ext1).expect("an external pid");
    let rpid2 = RemotePid::from_eterm(ext2).expect("an external pid");
    assert!(
        fx.table.link_term(dying, ext1).is_err(),
        "the transport is down, so the `DOP_LINK` itself fails too"
    );
    assert!(fx.table.link_term(dying, ext2).is_err());
    assert_eq!(
        fx.table.remote_links(dying),
        &[rpid1, rpid2],
        "the local record of a remote link survives a routing failure (only the wire signal is \
         dropped)"
    );

    let reason = make_small(7);
    fx.table
        .terminate_with_ports(&mut fx.ports, dying, reason)
        .expect("a down distribution seam must not abort the death");

    // The death happened, and the observer still fired exactly once — a step *after* the failing
    // remote step.
    assert!(!fx.table.contains(dying), "the process died");
    assert_eq!(
        deaths.borrow().as_slice(),
        &[dying],
        "the death observer ran despite the routing failure"
    );

    // Every remote link partner was attempted exactly once: the cascade kept going after the
    // first transport failure.
    let attempts = attempts.borrow();
    let exits: Vec<&DistSignal> = attempts
        .iter()
        .filter(|signal| matches!(signal, DistSignal::Exit { .. }))
        .collect();
    assert_eq!(
        exits.len(),
        2,
        "one exit signal per remote link partner, got {attempts:?}"
    );
    assert_eq!(
        *exits[0],
        DistSignal::Exit {
            from: dying,
            to: rpid1,
            reason,
        }
    );
    assert_eq!(
        *exits[1],
        DistSignal::Exit {
            from: dying,
            to: rpid2,
            reason,
        },
        "the second partner was still routed after the first failed"
    );
    drop(attempts);

    // The local watcher still got its `{'DOWN', …}` (step 2) ...
    assert_eq!(
        mailbox_len(&fx.table, watcher),
        1,
        "the local watcher is still told the target is down"
    );
    // ... and the port table no longer lists the dead watcher (step 6).
    assert!(
        fx.ports.monitors(port).is_empty(),
        "the dead watcher's incoming port monitor was still removed"
    );
}

/// A link peer whose heap cannot take the exit reason (a relocation-exempt process refusing to
/// collect/grow) must not abort the death: the *next* link partner is still signalled, and the
/// observer and port monitor cleanup still run.
#[test]
fn an_unreachable_peer_heap_does_not_abort_the_cascade() -> Result<()> {
    let mut fx = Fixture::new();
    let dying = fx.spawn();
    let exempt = fx.spawn();
    let trapping = fx.spawn();
    let watcher = fx.spawn();
    let owner = fx.spawn();
    // A trapping receiver survives the exit and queues it as a message, so the delivery is
    // observable rather than cascading into another death.
    fx.table
        .get_mut(trapping)
        .expect("trapping is live")
        .set_trap_exit(true);

    // Step 1, first peer: a relocation-exempt process — its nursery cannot be collected or grown,
    // so a copy that does not fit is refused raw (see `tests/signal_exhaustion.rs`).
    fx.table.link(dying, exempt)?;
    fx.table.link(dying, trapping)?;
    fx.table.set_relocation_exempt(exempt, true)?;
    // Step 2, step 6 and step 5: a watcher, an incoming port monitor, and the observer.
    fx.table.monitor(watcher, dying, MonitorKind::Process)?;
    let port = fx.port(owner);
    fx.table.monitor_port(dying, port, &mut fx.ports)?;
    let deaths = fx.record();

    // One word larger than the exempt peer's whole bounded nursery: the copy is refused and the
    // exempt receiver must not collect or grow to fit it.
    let mut src = Heap::new(4096).expect("the source arena allocates");
    let capacity = fx
        .table
        .get(exempt)
        .expect("exempt is live")
        .collector()
        .young()
        .capacity_words();
    let payload_words = capacity + 1;
    let elems: Vec<Eterm> = (0..payload_words.saturating_sub(1))
        .map(|i| make_small(i as i64))
        .collect();
    let reason = src
        .tuple(&elems)
        .expect("the oversized reason fits the source arena");
    assert_eq!(size_object(reason), payload_words, "header + elements");

    let base_before = fx
        .table
        .get(exempt)
        .expect("exempt is live")
        .collector()
        .young()
        .base();

    fx.table
        .terminate_with_ports(&mut fx.ports, dying, reason)
        .expect("an unreachable peer heap must not abort the death");

    assert!(!fx.table.contains(dying), "the process died");
    assert_eq!(
        deaths.borrow().as_slice(),
        &[dying],
        "the death observer ran despite the refused copy"
    );

    // The exempt peer refused the copy without collecting, growing or leaving partial state ...
    let exempt_process = fx.table.get(exempt).expect("the exempt peer is still live");
    assert_eq!(
        exempt_process.pending_signals(),
        0,
        "the refused exit signal was not queued"
    );
    assert_eq!(
        exempt_process.mailbox().len(),
        0,
        "nothing was delivered either"
    );
    assert_eq!(
        exempt_process.collector().young().base(),
        base_before,
        "a relocation-exempt receiver must not relocate (or grow) its nursery"
    );
    // ... and the *next* link partner was still signalled (step 1 kept going).
    assert!(
        fx.table.contains(trapping),
        "the trapping peer survived the exit it was sent"
    );
    assert_eq!(
        mailbox_len(&fx.table, trapping),
        1,
        "the second link partner was still signalled after the first refused"
    );
    // Step 2 ran.
    assert_eq!(
        mailbox_len(&fx.table, watcher),
        1,
        "the local watcher is still told the target is down"
    );
    // Step 6 ran.
    assert!(
        fx.ports.monitors(port).is_empty(),
        "the dead watcher's incoming port monitor was still removed"
    );
    Ok(())
}
