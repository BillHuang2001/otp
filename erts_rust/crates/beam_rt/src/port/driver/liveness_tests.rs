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

//! The A.4 **driver-monitor liveness wiring suite**: the runtime's published process-liveness view
//! ([`ProcessLiveness`]) reaching a real C driver's `driver_monitor_process` through the real
//! `#[no_mangle]` entry point.
//!
//! The C ground truth is `do_driver_monitor_process` (`io.c:7012-7038`): an entry with **no**
//! `process_exit` slot answers `-1`; a target that cannot be monitored because it is **not alive**
//! answers the **positive** `1` (`ERL_DRV_MONITOR_NOT_ALIVE`); otherwise the monitor is armed and
//! the answer is `0`. Until the three-way [`beam_abi::MonitorResult`] was threaded into
//! [`DriverPort`](super::DriverPort), this crate could only answer `0`/`-1`, so a C driver
//! monitoring a dead process got the wrong code.
//!
//! This suite is the wiring proof that closes that gap end to end: a **real** C driver, loaded
//! through the production `open_port({spawn_driver, …})` path, calls the real
//! `driver_monitor_process` entry (handed to it as a function pointer — a `--lib` unit-test link
//! exports no `driver_*` symbol), and the code it gets back is read from the driver's own reply
//! buffer.
//!
//! * a **live** pid (the runtime's last port round observed it) ⇒ `0`;
//! * a pid the last port round did **not** observe (removed from the live table) ⇒ `1`.
//!
//! # The liveness input
//!
//! The per-port host is built before the port is registered and holds no process table, so it
//! answers from the runtime's **published** observation ([`ProcessLiveness`]; refreshed at the
//! start of every port round by
//! [`PortRegistry::refresh_liveness`](crate::port::PortRegistry)). The control callback that calls
//! `driver_monitor_process` runs **outside** a round, so it sees the view the last round published
//! — which is exactly what this test drives: it runs a real port round between the two calls.
//!
//! # Skip policy
//!
//! Needs the host C compiler and the host `erl_driver.h`; without either it prints a **loud**
//! `SKIP:` banner and returns — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use beam_alloc::Heap;
use beam_bifs::PortHook;
use beam_process::{PortId, ProcessId, ProcessTable, SignalAtoms};
use beam_term::{make_atom, AtomTable, NIL};

use crate::config::RuntimeConfig;
use crate::port::RuntimePorts;
use crate::Runtime;

/// The fixture: a driver whose entry declares a `process_exit` slot (so `driver_monitor_process` is
/// live at all) **and** a `control` slot.
///
/// `start` records the marker path, the `ErlDrvPort`, and the `driver_monitor_process` address it is
/// handed in the `command` string (the loader passes the name's first space-delimited token as the
/// object path and the rest to the driver, so the object path is the first token and the two values
/// this fixture needs are the second and third).
///
/// The one control verb (`6`) reads the pid term's raw word out of the request buffer, casts it to
/// `ErlDrvTermData`, calls `driver_monitor_process` through the handed pointer, and writes the
/// returned code into the reply buffer — so the test observes the **real** entry point's answer.
const LIVENESS_C: &str = r#"
#include <erl_driver.h>

#include <stdio.h>
#include <string.h>

#define RT_MARKER_MAX 512

/* The control verb this test drives. */
#define RT_LIVENESS_MONITOR 6

typedef int (*rt_monitor_fn)(ErlDrvPort, ErlDrvTermData, ErlDrvMonitor *);

typedef struct {
    ErlDrvPort port;
    FILE *marker;
    rt_monitor_fn monitor;
} rt_state;

/* One port per loaded object, so the state can live in this object's own storage: the fixture
 * deliberately imports no `driver_*` symbol (a `--lib` unit-test link exports none). */
static rt_state rt_state_storage;

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    rt_state *st = &rt_state_storage;
    char object[RT_MARKER_MAX];
    char marker[RT_MARKER_MAX];
    unsigned long monitor_addr = 0;
    object[0] = '\0';
    marker[0] = '\0';
    if (command == NULL)
        return (ErlDrvData) -1;
    /* `<object path> <driver_monitor_process address> <marker path>`. */
    if (sscanf(command, "%511s %lx %511s", object, &monitor_addr, marker) != 3)
        return (ErlDrvData) -1;
    if (monitor_addr == 0 || marker[0] == '\0')
        return (ErlDrvData) -1;
    st->marker = fopen(marker, "w");
    if (st->marker == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->monitor = (rt_monitor_fn) (intptr_t) monitor_addr;
    return (ErlDrvData) st;
}

static void rt_stop(ErlDrvData drv_data) {
    rt_state *st = (rt_state *) drv_data;
    if (st != NULL && st->marker != NULL) {
        fclose(st->marker);
        st->marker = NULL;
    }
}

static ErlDrvSSizeT rt_control(ErlDrvData drv_data, unsigned int cmd, char *buf,
                               ErlDrvSizeT len, char **rbuf, ErlDrvSizeT rlen) {
    rt_state *st = (rt_state *) drv_data;
    ErlDrvTermData term;
    ErlDrvMonitor monitor;
    int code;
    if (st == NULL || st->monitor == NULL || cmd != RT_LIVENESS_MONITOR)
        return (ErlDrvSSizeT) -1;
    if (len < (ErlDrvSizeT) sizeof(ErlDrvTermData) || rlen < 1)
        return (ErlDrvSSizeT) -1;
    /* The pid term's raw word, handed over as the request bytes. */
    memcpy(&term, buf, sizeof(ErlDrvTermData));
    code = st->monitor(st->port, term, &monitor);
    if (st->marker != NULL) {
        fprintf(st->marker, "M:%d\n", code);
        fflush(st->marker);
    }
    /* One byte: the code the real entry point answered (0 / 1 / -1). */
    (*rbuf)[0] = (char) (unsigned char) code;
    return (ErlDrvSSizeT) 1;
}

static ErlDrvEntry rt_liveness_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_LIVENESS_NAME,
    .finish = NULL,
    .handle = NULL,
    .control = rt_control,
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

/* The entry must declare a `process_exit` slot: without one the C answers `-1` before the liveness
 * question is ever asked (`io.c:7020-7021`), which is the other (already pinned) branch. */
static void rt_process_exit(ErlDrvData drv_data, ErlDrvMonitor *monitor) {
    rt_state *st = (rt_state *) drv_data;
    (void) monitor;
    if (st != NULL && st->marker != NULL) {
        fprintf(st->marker, "P:\n");
        fflush(st->marker);
    }
}

DRIVER_INIT(rt_liveness_init) {
    return &rt_liveness_entry;
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
/// A compact duplicate of `fire_tests.rs::erl_include_dir` (a unit test cannot import the `tests/`
/// crate target's support).
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

/// The stem the fixture is compiled and loaded under (its `driver_name` must match).
const STEM: &str = "rt_liveness";

/// A compiled driver `.so` plus the marker file its `control` callback appends to.
struct Fixture {
    /// The scratch directory holding the source, the `.so` and the marker.
    dir: PathBuf,
    /// The `.so` **without** its suffix (the object the port's name points at).
    stem: PathBuf,
    /// The path the fixture records each control call in.
    marker: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output and the marker are not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The `M:<code>` lines the fixture has written so far, in order.
    fn codes(&self) -> Vec<String> {
        let text = match std::fs::read_to_string(&self.marker) {
            Ok(text) => text,
            // A missing marker means the fixture never got as far as `start`: report it as no lines
            // so the caller's assertion, not a panic, is what fails.
            Err(_) => return Vec::new(),
        };
        text.lines()
            .filter_map(|line| line.strip_prefix("M:"))
            .map(str::to_string)
            .collect()
    }
}

/// Compiles [`LIVENESS_C`] as `<scratch>/<stem>.so`; `None` when a host facility is missing (the
/// caller returns early — **loudly**).
fn build_fixture() -> Option<Fixture> {
    if !cc_available() {
        skip(&format!("liveness: no C compiler ({} / $CC) available", cc()));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("liveness: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_liveness_{STEM}_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{STEM}.c"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, LIVENESS_C));
    if let Err(error) = write {
        skip(&format!(
            "liveness: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    // `cc -shared -fPIC -DRT_LIVENESS_NAME="<stem>" -I<include> -o <dir>/<stem>.so <dir>/<stem>.c`
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_LIVENESS_NAME=\"{STEM}\""))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{STEM}.so")))
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "liveness: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("liveness: could not run the C compiler: {error}"));
            return None;
        }
    }
    if !dir.join(format!("{STEM}.so")).is_file() {
        skip(&format!("liveness: the C compiler produced no {STEM}.so"));
        return None;
    }
    Some(Fixture {
        stem: dir.join(STEM),
        marker: dir.join("marker.txt"),
        dir,
    })
}

// ---------------------------------------------------------------------------------------------
// Driving the port through the production `open_port({spawn_driver, …})` path
// ---------------------------------------------------------------------------------------------

/// The address of the real `driver_monitor_process` entry point, handed to the fixture.
fn monitor_address() -> usize {
    beam_abi::driver::drv::driver_api_table()
        .driver_monitor_process
        .expect("the `driver_monitor_process` entry point is registered in the table")
        as *const () as usize
}

/// Invokes the fixture's control verb for `pid` and returns the **reply bytes** (the code the real
/// entry point answered, as one byte).
fn monitor_code(rt: &Runtime, id: PortId, pid: ProcessId) -> Vec<u8> {
    let request = pid.to_eterm().raw().to_ne_bytes().to_vec();
    let cell = Rc::clone(&rt.ports);
    let mut registry = cell.try_borrow_mut().expect("the port registry is free");
    registry
        .port_control(id, "6", &request)
        .expect("the driver's `control` answers")
}

/// The wiring proof, end to end.
///
/// Discriminates: before the three-way hook was threaded through, the host could only answer `0` or
/// `-1`, so the second assertion (`1` for the removed pid) fails on the old shape; and an
/// `open_port` arm that did not hand the host the published view (`with_liveness`) would leave the
/// host with no observation, so a dead pid would be armed and answer `0` — also failing here.
#[test]
fn a_dead_local_process_answers_the_cs_positive_not_alive() {
    let Some(fixture) = build_fixture() else {
        return;
    };
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");

    let mut atoms = AtomTable::new();
    let tag = make_atom(atoms.intern("spawn_driver"));
    let atoms = Rc::new(atoms);
    let mut heap = Heap::new(1024).expect("a test heap");
    // The production `Name`: the first space-delimited token is the object to load; the rest is the
    // command the driver's `start` receives (`<object> <monitor address> <marker path>`).
    let command = format!(
        "{} {:x} {}",
        fixture.stem.with_extension("so").display(),
        monitor_address(),
        fixture.marker.display()
    );
    let command = crate::port::charlist(&mut heap, &command).expect("a command string");
    let name = heap.tuple(&[tag, command]).expect("a name tuple");

    let owner = ProcessId::new(1, 0);
    let mut hook = RuntimePorts::new(Rc::clone(&rt.ports), Rc::clone(&atoms), owner);
    let id = hook
        .open_port(name, NIL)
        .expect("the fixture driver loads and registers through the production path");

    // A live process and one that will be gone by the next port round.
    let mut table = ProcessTable::new(SignalAtoms::from_table(rt.code_server_atoms_mut()));
    let alive = table.spawn_default().expect("a live process");
    let dead = table.spawn_default().expect("a second process");
    assert!(table.take(dead).is_some(), "the second process is removed");
    assert!(table.contains(alive) && !table.contains(dead));

    // One real port round: this is what publishes the liveness view the host consults (the control
    // callback runs outside a round, so it sees *this* round's observation).
    rt.deliver_port_messages(&mut table, Duration::from_millis(1))
        .expect("the port round runs");

    assert_eq!(
        monitor_code(&rt, id, alive),
        vec![0u8],
        "a process the round observed alive is armed: the C's ERL_DRV_MONITOR_OK (0)"
    );
    assert_eq!(
        monitor_code(&rt, id, dead),
        vec![1u8],
        "a process the round did not observe answers the C's positive \
         ERL_DRV_MONITOR_NOT_ALIVE (1), not the -1 the two-way subset could express"
    );
    assert_eq!(fixture.codes(), vec!["0".to_string(), "1".to_string()]);

    // A not-alive answer installs nothing: no monitor on the dead pid may sit in the tree waiting
    // for a `process_exit` that can never come.
    let monitors = {
        let cell = Rc::clone(&rt.ports);
        let registry = cell.try_borrow().expect("the port registry is free");
        registry
            .driver_monitors(id)
            .expect("a C driver port seizes its driver's monitor tree")
    };
    assert!(
        !monitors.monitors(dead),
        "the dead pid's monitor was never installed"
    );
    assert!(
        monitors.monitors(alive),
        "the live pid's monitor is the one that was installed"
    );

    hook.close_port(id).expect("the port closes");
    fixture.cleanup();
}

/// A control callback that runs **before any port round** sees no observation, so a local pid is
/// monitored anyway — the documented non-regression fallback (never a fabricated `NotAlive`).
///
/// Discriminates: the `open_port` itself must leave the view unobserved (`None`); an implementation
/// that cleared the view to an *empty* set instead of `None` at construction would answer `1` here.
#[test]
fn a_callback_before_any_round_monitors_anyway() {
    let Some(fixture) = build_fixture() else {
        return;
    };
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");

    let mut atoms = AtomTable::new();
    let tag = make_atom(atoms.intern("spawn_driver"));
    let atoms = Rc::new(atoms);
    let mut heap = Heap::new(1024).expect("a test heap");
    let command = format!(
        "{} {:x} {}",
        fixture.stem.with_extension("so").display(),
        monitor_address(),
        fixture.marker.display()
    );
    let command = crate::port::charlist(&mut heap, &command).expect("a command string");
    let name = heap.tuple(&[tag, command]).expect("a name tuple");

    let owner = ProcessId::new(1, 0);
    let mut hook = RuntimePorts::new(Rc::clone(&rt.ports), Rc::clone(&atoms), owner);
    let id = hook
        .open_port(name, NIL)
        .expect("the fixture driver loads and registers through the production path");

    let mut table = ProcessTable::new(SignalAtoms::from_table(rt.code_server_atoms_mut()));
    let pid = table.spawn_default().expect("a live process");

    // No port round has run, so nothing has been published.
    assert_eq!(
        monitor_code(&rt, id, pid),
        vec![0u8],
        "with no observation the monitor is installed (the C's 0 for a live target)"
    );

    hook.close_port(id).expect("the port closes");
    fixture.cleanup();
}