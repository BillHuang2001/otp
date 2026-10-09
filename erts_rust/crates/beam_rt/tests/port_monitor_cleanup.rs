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

//! **Dead-watcher port-monitor reclamation** (contract A.4) at the [`Runtime`] boundary.
//!
//! `erlang:monitor(port, Port)` records an *incoming* monitor on the port (so a port close can
//! deliver `{'DOWN', Ref, port, Port, Reason}` to the watcher) **and** an *outgoing* entry on
//! the watching process. When the **watcher** dies first, that incoming entry outlives it, and
//! `beam_process`'s finalizer cannot remove it: the incoming half lives on the port table, and
//! this runtime threads no `beam_process::PortTable` into the termination path
//! ([`ProcessTable::terminate_with_ports`](beam_process::ProcessTable::terminate_with_ports) is
//! the entry point that can). Without the sweep, a port would keep a monitor for a process that
//! no longer exists, forever.
//!
//! The runtime closes that gap in two steps (`src/runtime/ets_owner.rs`): its
//! [`ProcessExitObserver`](beam_process::ProcessExitObserver) records each dying watcher's
//! drained `ProcessExit::port_monitors()` pairs, and a scheduler-pass round drains them
//! ([`Runtime::drain_port_monitor_exits`]) by `PortTable::demonitor(port, reference)`.
//!
//! # Why the observable is white-box
//!
//! The oracle cannot witness this. `erlang:port_info(Port, monitors)` always answers `[]` — even
//! with a live watcher — so no `-eval`/CLI-level program can tell a swept entry from a dangling
//! one. The authoritative set is the runtime's own `beam_process::PortTable`, read here through
//! the public [`Runtime::port_monitors`] (there is no other public path to it: the registry sits
//! behind a private `Rc<RefCell<PortRegistry>>`). `erlang:port_info/1,2` is therefore
//! deliberately **not** touched by the sweep.
//!
//! # The port, and the two watcher shapes
//!
//! The port is opened through the `-eval` evaluator (`open_port({spawn, "cat"}, [])`), which is
//! the only route from an integration test to a real `PortId`; its owner is this runtime's root
//! process, which stays alive, so the port stays live. The watcher is a compiled fixture
//! (`tests/fixtures/port_watch.erl`, built at test time with the system `erlc`): a `-eval`
//! `fun () -> … end` closure cannot park in a `receive`, and the point of the test is to observe
//! the incoming entry of a **live** watcher before the death — which is what makes the empty
//! post-state discriminate rather than pass for the wrong reason (a monitor that was never
//! recorded would also read as empty).
//!
//! A missing `erlc` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery and
// loud-skip banner (no oracle probe: the oracle cannot answer this question).
#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_process::{PortId, ProcessId};
use beam_rt::Runtime;

/// The fixture **source** (compiled at test time into a scratch directory — never committed as a
/// `.beam`, so nothing platform-dependent enters the tree).
const FIXTURE_SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/port_watch.erl");

/// The fixture module.
const MODULE: &str = "port_watch";

// ---------------------------------------------------------------------------------------
// The compiler
// ---------------------------------------------------------------------------------------

/// Resolves `name` (`erlc`) from a sibling of `$ERTS_ORACLE` first (the same installation), then
/// from `$PATH` — a filesystem question, never a probe that could block.
fn find_tool(name: &str) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let oracle = PathBuf::from(&value);
        if let Some(dir) = oracle.parent() {
            let candidate = dir.join(name);
            if support::is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    support::find_on_path(name)
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// The compiled fixture module in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding `port_watch.erl` and `port_watch.beam`; it is the single
    /// `-pa` this runtime is given, and it never holds a `lists.beam`.
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test, yet a
        // cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compiles the committed `port_watch.erl` into a scratch directory, or `None` after a **loud**
/// skip when the system toolchain is unavailable.
fn build_fixture() -> Option<Fixture> {
    let Some(erlc) = find_tool("erlc") else {
        support::skip_loudly(
            "port_monitor_cleanup",
            "no `erlc` on $PATH (and none beside $ERTS_ORACLE)",
        );
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_port_monitor_{}_{n}",
        std::process::id()
    ));
    let source = match std::fs::read(FIXTURE_SOURCE) {
        Ok(bytes) => bytes,
        Err(error) => {
            support::skip_loudly(
                "port_monitor_cleanup",
                &format!("cannot read {FIXTURE_SOURCE}: {error}"),
            );
            return None;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join("port_watch.erl"), &source))
    {
        support::skip_loudly(
            "port_monitor_cleanup",
            &format!("cannot prepare {}: {error}", dir.display()),
        );
        return None;
    }
    let output = match Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg("port_watch.erl")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(
                "port_monitor_cleanup",
                &format!("cannot run `erlc`: {error}"),
            );
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            "port_monitor_cleanup",
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    if !dir.join("port_watch.beam").is_file() {
        support::skip_loudly(
            "port_monitor_cleanup",
            &format!("`erlc` produced no {}", dir.join("port_watch.beam").display()),
        );
        return None;
    }
    Some(Fixture { dir })
}

// ---------------------------------------------------------------------------------------
// The shared setup
// ---------------------------------------------------------------------------------------

/// A booted runtime with the fixture on its code path and one **live** port opened on it.
///
/// The port's owner is the runtime's root process (the `-eval` self), which stays alive, so the
/// port stays live in the registry for the whole test.
fn runtime_with_port(fixture: &Fixture) -> (Runtime, PortId) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.add_code_path(&fixture.dir);
    let port_term = rt
        .eval("open_port({spawn, \"cat\"}, [])")
        .expect("the eval opens a live port");
    let port = PortId::from_eterm(port_term).expect("the eval returned a port term");
    (rt, port)
}

/// The watcher pids monitoring `port`, sorted so an assertion does not depend on insertion
/// order.
fn watchers(rt: &Runtime, port: PortId) -> Vec<ProcessId> {
    let mut pids = rt.port_monitors(port).expect("the port table is readable");
    pids.sort_unstable();
    pids
}

/// Spawns `port_watch:watch/1` on `port` and runs a pass, so the watcher has recorded its
/// monitor and parked in its `receive`.
fn spawn_watcher(rt: &mut Runtime, port_term: beam_term::Eterm) -> ProcessId {
    let pid = rt
        .spawn_process(MODULE, "watch", vec![port_term])
        .expect("spawn port_watch:watch/1");
    rt.run_scheduler().expect("the pass parks the watcher");
    pid
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// A watcher that monitors a **live** port and then dies leaves no entry on the port's monitor
/// set — and the entry it *did* record is asserted non-empty first, so an empty answer cannot
/// come from a monitor that was never recorded (or from a port that was never live).
///
/// The watcher parks after `erlang:monitor(port, Port)`, so the incoming entry is observed while
/// the watcher is still alive; a second fixture process then kills it, and the *next* scheduler
/// pass (the one that runs the killer) sweeps the dangling entry.
#[test]
fn a_dead_watchers_port_monitor_is_swept() {
    let Some(fixture) = build_fixture() else {
        return;
    };
    let (mut rt, port) = runtime_with_port(&fixture);
    let port_term = port.to_eterm();

    // PRE: nothing watches the port yet.
    assert!(
        watchers(&rt, port).is_empty(),
        "no watcher exists before the watcher is spawned"
    );

    // The watcher records its monitor and parks — the PRE state the sweep must clear.
    let doomed = spawn_watcher(&mut rt, port_term);
    assert_eq!(
        watchers(&rt, port),
        vec![doomed],
        "the live watcher's incoming port monitor is recorded while it is parked"
    );
    assert!(
        rt.processes().get(doomed).is_some(),
        "the watcher parked in its receive rather than dying"
    );

    // Its death: a second fixture process kills it untrappably.
    rt.spawn_process(MODULE, "kill", vec![doomed.to_eterm()])
        .expect("spawn port_watch:kill/1");
    rt.run_scheduler()
        .expect("the pass kills the watcher and sweeps the dangling monitor");

    assert!(
        rt.processes().get(doomed).is_none(),
        "the watcher really terminated (otherwise the empty set below proves nothing)"
    );
    assert!(
        watchers(&rt, port).is_empty(),
        "the dead watcher left no incoming port monitor behind"
    );
}

/// A monitor held by a **surviving** process is *not* swept: the drain removes exactly the pairs
/// the dead watcher recorded, never the whole set.
///
/// The doomed watcher's entry disappears while the survivor's entry — recorded on the same live
/// port, through the same `erlang:monitor/2` path — stays. So the assertion is not "the set is
/// empty" but "the set is exactly the live watcher", which is what discriminates a targeted
/// sweep from a blanket clear.
#[test]
fn a_surviving_watchers_port_monitor_is_kept() {
    let Some(fixture) = build_fixture() else {
        return;
    };
    let (mut rt, port) = runtime_with_port(&fixture);
    let port_term = port.to_eterm();

    // Two watchers on the same live port: one that will die, one that stays.
    let survivor = spawn_watcher(&mut rt, port_term);
    let doomed = spawn_watcher(&mut rt, port_term);
    let mut both = vec![survivor, doomed];
    both.sort_unstable();
    assert_eq!(
        watchers(&rt, port),
        both,
        "both live watchers' incoming port monitors are recorded"
    );

    rt.spawn_process(MODULE, "kill", vec![doomed.to_eterm()])
        .expect("spawn port_watch:kill/1");
    rt.run_scheduler()
        .expect("the pass kills one watcher and sweeps only its monitor");

    assert!(
        rt.processes().get(doomed).is_none(),
        "the doomed watcher terminated"
    );
    assert!(
        rt.processes().get(survivor).is_some(),
        "the survivor is still alive and still watching"
    );
    assert_eq!(
        watchers(&rt, port),
        vec![survivor],
        "only the dead watcher's monitor was removed"
    );
}
