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

//! The A.4 **`process_exit` firing regression suite** for
//! [`DriverPortAdapter::has_pending_async`](super::DriverPortAdapter::has_pending_async) /
//! [`DriverPortAdapter::drive_async`](super::DriverPortAdapter::drive_async) — the per-round drive
//! point that hands a fired driver monitor to the entry's `ErlDrvEntry::process_exit` callback.
//!
//! # What is pinned, and where
//!
//! The runtime side that *marks* a monitor pending (the process-exit observer / scheduler-pass drain)
//! is a higher scope's obligation, so every case here marks through the **shared**
//! [`DriverMonitors`] handle the port seizes from the host — exactly the handle that scope will hold.
//! The observable is the driver's own callback: a real C driver compiled at test time whose
//! `process_exit` writes a marker-file line carrying the first 8 bytes of the `ErlDrvMonitor` the
//! runtime handed it.
//!
//! * **fire once** — the port reports `has_pending_async() == true` for a marked monitor, a real
//!   `beam_io` port-task round runs the drive point, the fixture writes its marker exactly once
//!   (with the very identity the suite marked), and the second round writes nothing;
//! * **count and order** — two marked monitors fire in one `drive_async`, which reports `2`, each
//!   exactly once;
//! * **a fired monitor is gone** — nothing left pending and the identity no longer resolves (the
//!   state `driver_get_monitored_process` answers from);
//! * **teardown** — after [`beam_io::Driver::stop`] nothing is pending and no later round writes a
//!   marker; the same for an adapter dropped without `stop`;
//! * **no spurious drive point** — a fresh port with no monitors answers `false`.
//!
//! # How the fixture reaches what it needs
//!
//! The same constraint `ready_output_tests.rs` documents: a `--lib` unit-test link is not given
//! `-Wl,--export-dynamic`, so the fixture imports **no** `driver_*` symbol — and needs none, because
//! `process_exit` receives its `ErlDrvData` state and the `ErlDrvMonitor *` as arguments. The one
//! thing it cannot get from a callback is the marker-file path, which the test hands it in the
//! loader's `command` string.
//!
//! # Skip policy
//!
//! Needs the host C compiler and the host `erl_driver.h`; without either it prints a **loud**
//! `SKIP:` banner and returns — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_abi::{monitor_from_identity, monitor_identity, ErlDrvMonitor};
use beam_alloc::Heap;
use beam_io::registry::PortRegistry;
use beam_io::Poller;
use beam_process::{PortId, ProcessId};
use beam_term::AtomTable;

use super::{DriverMonitors, DriverPort, DriverPortAdapter};

/// The fixture: a driver whose entry declares a `process_exit` slot and records every firing in a
/// marker file.
///
/// One line per call: `P:` followed by the first 8 bytes of the `ErlDrvMonitor` the runtime passed,
/// hex-encoded. That is the identity this runtime mints per monitor, so the suite can assert that
/// the callback received **the very monitor** it marked — not a fabricated or mixed-up one. The
/// fixture imports no `driver_*` symbol (see the module docs).
const FIRE_C: &str = r#"
#include <erl_driver.h>

#include <fcntl.h>
#include <stdio.h>

#define RT_MARKER_MAX 512

typedef struct {
    int marker_fd;
    ErlDrvPort port;
} rt_state;

/* One port per loaded object, so the state can live in this object's own storage: the fixture
 * deliberately imports neither `driver_alloc`/`driver_free` nor any `driver_*` symbol. */
static rt_state rt_state_storage;

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    rt_state *st = &rt_state_storage;
    char marker[RT_MARKER_MAX];
    marker[0] = '\0';
    if (command == NULL)
        return (ErlDrvData) -1;
    if (sscanf(command, "%511s", marker) != 1 || marker[0] == '\0')
        return (ErlDrvData) -1;
    st->marker_fd = open(marker, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (st->marker_fd < 0)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void rt_stop(ErlDrvData drv_data) {
    (void) drv_data;
}

/* The callback under test. It is handed the driver's private state and the monitor the runtime
 * fired, and needs nothing else. */
static void rt_process_exit(ErlDrvData drv_data, ErlDrvMonitor *monitor) {
    rt_state *st = (rt_state *) drv_data;
    int i;
    if (st == NULL || st->marker_fd < 0)
        return;
    if (monitor == NULL) {
        dprintf(st->marker_fd, "P:null\n");
        return;
    }
    dprintf(st->marker_fd, "P:");
    for (i = 0; i < 8; i++)
        dprintf(st->marker_fd, "%02x", (unsigned) monitor->data[i]);
    dprintf(st->marker_fd, "\n");
}

static ErlDrvEntry rt_fire_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_FIRE_NAME,
    .finish = NULL,
    .handle = NULL,
    .control = NULL,
    .timeout = NULL,
    .outputv = NULL,
    .ready_async = NULL,
    .flush = NULL,
    .call = NULL,
    .unused_event_callback = NULL,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
    .driver_flags = 0,
    .handle2 = NULL,
    .process_exit = rt_process_exit,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(rt_fire_init) {
    return &rt_fire_entry;
}
"#;

/// Print a loud skip banner (never a silent pass).
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

// ---------------------------------------------------------------------------------------------
// Locating the host `erl_driver.h`
// ---------------------------------------------------------------------------------------------

/// The header the include-directory search keys on.
const HEADER: &str = "erl_driver.h";

/// The host `erl_driver.h` include directory, or `None` when it cannot be found.
///
/// A compact duplicate of `ready_output_tests.rs::erl_include_dir` (a unit test cannot import the
/// `tests/` crate target's support). Resolution order: ask the host `erl`; then the `include`
/// sibling of the `erts-*` directory the `erl` on `$PATH` lives in; then the standard roots.
fn erl_include_dir() -> Option<PathBuf> {
    from_erl_eval().or_else(from_path_erl).or_else(|| {
        let mut roots = Vec::new();
        if let Some(top) = std::env::var_os("ERL_TOP") {
            roots.push(PathBuf::from(top));
        }
        for base in [
            "/usr/lib/erlang",
            "/usr/local/lib/erlang",
            "/usr/lib64/erlang",
            "/opt/erlang",
        ] {
            roots.push(PathBuf::from(base));
        }
        roots.into_iter().find_map(|root| search_root(&root))
    })
}

/// True when `dir` holds a readable `erl_driver.h`.
fn has_header(dir: &Path) -> bool {
    dir.join(HEADER).is_file()
}

/// Ask the host `erl` for `code:lib_dir(erts, include)`.
fn from_erl_eval() -> Option<PathBuf> {
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir(erts,include)])",
            "-s",
            "init",
            "stop",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    has_header(&dir).then_some(dir)
}

/// The `include` sibling of the `erts-*` directory the `erl` on `$PATH` lives in.
fn from_path_erl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let erl = std::env::split_paths(&path)
        .map(|dir| dir.join("erl"))
        .find(|candidate| candidate.is_file())?
        .canonicalize()
        .ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    has_header(&include).then_some(include)
}

/// Search one installation root for an include directory holding `erl_driver.h`.
fn search_root(root: &Path) -> Option<PathBuf> {
    let direct = root.join("include");
    if has_header(&direct) {
        return Some(direct);
    }
    let lib_erlang = root.join("lib").join("erlang");
    for base in [root.to_path_buf(), lib_erlang.clone()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("erts-") {
                continue;
            }
            let include = entry.path().join("include");
            if has_header(&include) {
                return Some(include);
            }
        }
    }
    let include = lib_erlang.join("include");
    has_header(&include).then_some(include)
}

// ---------------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------------

/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Whether a C compiler is available.
fn cc_available() -> bool {
    Command::new(cc())
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// A compiled driver `.so` plus the marker file its `process_exit` writes to.
struct Fixture {
    /// The scratch directory holding the source, the `.so` and the marker.
    dir: PathBuf,
    /// The `.so` **without** its suffix (the `DriverPortAdapter::load` candidate).
    stem: PathBuf,
    /// The path the fixture records each firing in.
    marker: PathBuf,
    /// The `driver_name` the library is compiled under (the loader rejects a mismatch).
    name: String,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output and the marker are not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The eight identity bytes of every `process_exit` firing so far, hex-encoded.
    ///
    /// Read after the callback has returned — the fixture `dprintf`s each line as it happens, so no
    /// flush is needed (and a partial line cannot be observed).
    fn firings(&self) -> Vec<String> {
        let text = match std::fs::read_to_string(&self.marker) {
            Ok(text) => text,
            // A missing marker means the fixture never got as far as `start`: report it as no
            // firings so the caller's assertion, not a panic, is what fails.
            Err(_) => return Vec::new(),
        };
        text.lines()
            .filter_map(|line| line.strip_prefix("P:"))
            .map(|value| value.trim().to_string())
            .collect()
    }
}

/// The eight identity bytes of `identity`, hex-encoded — the shape the fixture records.
fn hex8(identity: [u8; 24]) -> String {
    identity[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The 24-byte identity of the holder an install returned.
fn identity_of(monitor: &ErlDrvMonitor) -> [u8; 24] {
    monitor_identity(monitor as *const ErlDrvMonitor)
        .expect("a monitor this suite built is non-null")
}

/// Compiles [`FIRE_C`] as `<scratch>/<stem>.so`; `None` when a host facility is missing (the
/// caller returns early — **loudly**).
fn build_fixture(stem: &str) -> Option<Fixture> {
    if !cc_available() {
        skip(&format!(
            "process_exit: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("process_exit: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_process_exit_{stem}_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{stem}.c"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, FIRE_C));
    if let Err(error) = write {
        skip(&format!(
            "process_exit: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    // `cc -shared -fPIC -DRT_FIRE_NAME="<stem>" -I<include> -o <dir>/<stem>.so <dir>/<stem>.c`
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_FIRE_NAME=\"{stem}\""))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{stem}.so")))
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "process_exit: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "process_exit: could not run the C compiler: {error}"
            ));
            return None;
        }
    }
    if !dir.join(format!("{stem}.so")).is_file() {
        skip(&format!(
            "process_exit: the C compiler produced no {stem}.so"
        ));
        return None;
    }
    Some(Fixture {
        stem: dir.join(stem),
        marker: dir.join("marker.txt"),
        dir,
        name: stem.to_string(),
    })
}

// ---------------------------------------------------------------------------------------------
// Driving the port
// ---------------------------------------------------------------------------------------------

/// A loaded fixture driver, the shared monitor tree the port fires through, and the adapter the
/// caller may either register (through [`open_port`]) or drop directly.
struct Loaded {
    /// The compiled driver and its marker file.
    fixture: Fixture,
    /// The handle the port seizes from the host at `load` — what the runtime's exit observer holds.
    monitors: Rc<DriverMonitors>,
    /// The port lifecycle, not yet registered.
    adapter: DriverPortAdapter,
}

/// Compiles the fixture and loads it with a host whose monitor tree the caller can drive.
fn load(stem: &str) -> Option<Loaded> {
    let fixture = build_fixture(stem)?;
    // The same anchor `crate::port::PortRegistry::new` uses.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let atoms = Rc::new(AtomTable::new());
    let host = DriverPort::new(
        Heap::new(256).expect("a test heap"),
        atoms,
        ProcessId::new(1, 0),
    );
    // Seized before the host is consumed by `load`, and shared with the boxed host by `Rc`, so this
    // is the very tree the port's drive point reads.
    let monitors = host.monitors();
    let adapter = DriverPortAdapter::load(
        &[fixture.stem.display().to_string()],
        &fixture.name,
        host,
        fixture.marker.display().to_string(),
    )
    .expect("the fixture driver loads");
    Some(Loaded {
        fixture,
        monitors,
        adapter,
    })
}

/// A loaded fixture driver registered as one live `beam_io` port.
struct Open {
    /// The compiled driver and its marker file.
    fixture: Fixture,
    /// The handle the runtime's exit observer holds (see [`Loaded::monitors`]).
    monitors: Rc<DriverMonitors>,
    /// The poller the port is registered with.
    poller: Poller,
    /// The registry the port lives in.
    ports: PortRegistry,
    /// The port's id.
    id: PortId,
}

impl Open {
    /// One real port-task round.
    fn round(&mut self) {
        // The round is what consults `has_pending_async` and calls `drive_async`; its notification
        // list is not what this suite observes (the fixture emits no port data).
        self.ports
            .execute_port_tasks(&mut self.poller, 1024)
            .expect("the port-task round runs");
    }

    /// Whether the registered port asks for a drive point.
    fn has_pending_async(&mut self) -> bool {
        self.ports
            .get_mut(self.id)
            .expect("the port is live")
            .has_pending_async()
    }
}

/// Loads the fixture and opens one port on it.
fn open_port(stem: &str) -> Option<Open> {
    let Loaded {
        fixture,
        monitors,
        adapter,
    } = load(stem)?;
    let mut poller = Poller::new().expect("a poller");
    let mut ports = PortRegistry::new();
    let id = ports
        .open(&mut poller, Box::new(adapter))
        .expect("the port opens (the fixture's `start` recorded its marker file)");
    Some(Open {
        fixture,
        monitors,
        poller,
        ports,
        id,
    })
}

// ---------------------------------------------------------------------------------------------
// The discriminating cases
// ---------------------------------------------------------------------------------------------

/// A marked monitor is handed to the driver's `process_exit` **once**, by a real port-task round,
/// and never again — the C's "fire the monitor, unlink it" (`io.c:7140-7160`).
///
/// Discriminates: with no drive point (the state before this change) the port would report nothing
/// pending, no round would ever enter `drive_async`, and the marker file would stay empty.
#[test]
fn a_marked_monitor_fires_process_exit_once() {
    let Some(mut open) = open_port("rt_fire_once") else {
        return;
    };
    let marked = ProcessId::new(42, 7);
    let monitor = open.monitors.install(marked);
    let identity = identity_of(&monitor);
    let expected = hex8(identity);

    assert!(
        !open.has_pending_async(),
        "a monitor that has not exited must not ask for a drive point"
    );
    assert_eq!(
        open.monitors.note_process_exit(marked),
        1,
        "the tree marks the port's monitor on the exiting pid"
    );
    assert!(
        open.has_pending_async(),
        "the port asks for a drive point once a monitor is pending"
    );

    open.round();
    assert_eq!(
        open.fixture.firings(),
        vec![expected.clone()],
        "`process_exit` ran exactly once, with the very monitor that was marked"
    );

    // The monitor was unlinked as it fired, so the next round has nothing to drive.
    assert!(!open.has_pending_async(), "nothing is left pending");
    open.round();
    assert_eq!(
        open.fixture.firings(),
        vec![expected],
        "a second round must not fire the same monitor again"
    );

    // And it is gone from the tree — the state `driver_get_monitored_process` answers from.
    assert!(open.monitors.find(identity).is_none());
    assert!(!open.monitors.monitors(marked));

    let Open {
        fixture,
        mut poller,
        mut ports,
        id,
        ..
    } = open;
    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// Two marked monitors fire in **one** `drive_async`, which reports `2`, each exactly once and in
/// installation order.
///
/// Discriminates: a drive point that fired only the first (or one callback per round) would report
/// `1` and leave the second monitor pending — and a `take_pending` that yielded the same identity
/// twice would show as a duplicate marker line.
#[test]
fn two_marked_monitors_fire_in_one_drive_each_once() {
    let Some(mut open) = open_port("rt_fire_two") else {
        return;
    };
    let first = ProcessId::new(51, 0);
    let second = ProcessId::new(52, 0);
    let first_monitor = open.monitors.install(first);
    let second_monitor = open.monitors.install(second);
    let first_hex = hex8(identity_of(&first_monitor));
    let second_hex = hex8(identity_of(&second_monitor));
    assert_ne!(first_hex, second_hex, "each install mints a fresh identity");

    assert_eq!(open.monitors.note_process_exit(first), 1);
    assert_eq!(open.monitors.note_process_exit(second), 1);

    // Driven directly so the round's return value is observable: the number of callbacks fired.
    let fired = {
        let Open {
            poller, ports, id, ..
        } = &mut open;
        ports
            .get_mut(*id)
            .expect("the port is live")
            .drive_async(poller)
            .expect("the drive point runs")
    };
    assert_eq!(fired, 2, "both pending monitors fired in one drive");
    assert_eq!(
        open.fixture.firings(),
        vec![first_hex, second_hex],
        "installation order, each identity exactly once"
    );
    assert!(!open.monitors.has_pending());

    let Open {
        fixture,
        mut poller,
        mut ports,
        id,
        ..
    } = open;
    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// Teardown through [`beam_io::Driver::stop`] empties the tree: nothing stays pending and no later
/// round writes a marker — the port detaches its monitors as it goes away
/// (`erts_detach_processes`, `io.c:3846-3959`).
///
/// Discriminates: without the teardown guard the monitor would still be marked pending after the
/// port closed, and a drive point reached later (before the adapter was reclaimed) would fire
/// `process_exit` against a driver whose `finish`/`dlclose` had already run.
#[test]
fn stop_empties_the_monitor_tree_and_nothing_fires_afterwards() {
    let Some(mut open) = open_port("rt_fire_stop") else {
        return;
    };
    let marked = ProcessId::new(61, 0);
    let monitor = open.monitors.install(marked);
    let identity = identity_of(&monitor);
    assert_eq!(open.monitors.note_process_exit(marked), 1);
    assert!(open.monitors.has_pending());
    assert!(open.has_pending_async());

    {
        let Open {
            poller, ports, id, ..
        } = &mut open;
        ports.close(poller, *id).expect("the port closes");
    }

    assert!(
        !open.monitors.has_pending(),
        "`stop` emptied the tree, so no monitor can fire later"
    );
    assert!(open.monitors.find(identity).is_none());
    assert!(!open.monitors.monitors(marked));

    // A round after the close has no live port to drive at all.
    open.round();
    assert!(
        open.fixture.firings().is_empty(),
        "no marker may be written after teardown: {:?}",
        open.fixture.firings()
    );

    let Open { fixture, .. } = open;
    fixture.cleanup();
}

/// An adapter dropped **without** `stop` empties the tree too, before the library's own drop runs
/// its `finish`/`dlclose`.
///
/// Discriminates: the `Drop` body is the only point that is reliably before the `driver` field's
/// drop, so a guard placed anywhere else (or nowhere) would leave a pending monitor alive across the
/// unload.
#[test]
fn dropping_a_port_without_stop_empties_the_monitor_tree() {
    let Some(Loaded {
        fixture,
        monitors,
        adapter,
    }) = load("rt_fire_drop")
    else {
        return;
    };
    let marked = ProcessId::new(71, 0);
    let monitor = monitors.install(marked);
    let identity = identity_of(&monitor);
    assert_eq!(monitors.note_process_exit(marked), 1);
    assert!(monitors.has_pending());

    drop(adapter);

    assert!(
        !monitors.has_pending(),
        "the `Drop` guard emptied the tree before the library was unloaded"
    );
    assert!(monitors.find(identity).is_none());
    assert!(!monitors.monitors(marked));
    assert!(fixture.firings().is_empty(), "nothing fired");

    fixture.cleanup();
}

/// A fresh port with no monitors answers `false`, so it never gets a spurious drive point — and an
/// installed-but-live monitor does not either.
#[test]
fn a_port_with_no_pending_monitor_is_never_driven() {
    let Some(mut open) = open_port("rt_fire_fresh") else {
        return;
    };
    assert!(
        !open.has_pending_async(),
        "a fresh port has no pending monitor"
    );
    // A monitor for a process that has *not* exited is not pending either.
    let live = open.monitors.install(ProcessId::new(81, 0));
    assert!(open.monitors.monitors(ProcessId::new(81, 0)));
    assert!(!open.has_pending_async());

    open.round();
    assert!(
        open.fixture.firings().is_empty(),
        "no drive point, no callback: {:?}",
        open.fixture.firings()
    );

    // A holder the port never installed is not a monitor this port can fire.
    let stranger = monitor_from_identity([0x5a; 24]);
    assert!(open.monitors.find(identity_of(&stranger)).is_none());
    // ...while the one it did install still resolves.
    assert_eq!(
        open.monitors.find(identity_of(&live)),
        Some(ProcessId::new(81, 0))
    );

    let Open {
        fixture,
        mut poller,
        mut ports,
        id,
        ..
    } = open;
    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}
