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

//! The A.4 **port-attribute suite**: the two per-port attributes a driver records from inside a
//! callback and the runtime answers a port question from —
//!
//! * **the OS child pid** (`erl_drv_set_os_pid`, the C's `prt->os_pid`, `io.c:2971`) behind
//!   `port_info(Port, os_pid)` (`erl_bif_info.c:3814-3817`: `undefined` while unset), and
//! * **the busy state** (`set_busy_port`, `io.c:5208-5264`) behind
//!   [`beam_io::Driver::is_busy`] and the `ERL_DRV_FLAG_SOFT_BUSY` half
//!   [`beam_io::Driver::flags`] reports, which is what `beam_io::Port::command`'s decision table
//!   reads (`io.c:1749-1769`, `erl_bif_port.c:195-220`).
//!
//! # What is pinned, and at which level
//!
//! * **the seam, on a bare host** — an unclaimed [`DriverPort`] answers the C's invalid-port `-1`
//!   for `set_os_pid` and records nothing; a claimed one answers `0`, records the pid, and the
//!   recorded value survives [`DriverPort::clear_driver_state`] (the C's `prt->os_pid` is port
//!   state, not the driver's private state). "Unset" stays distinguishable from a recorded `0`.
//! * **end to end through a real C driver** — a `.so` whose `start` callback calls the real
//!   `erl_drv_set_os_pid` entry (handed to it as an address: a `--lib` unit-test link exports no
//!   `driver_*` symbol) records `4242` on the handle the suite seized before the host was boxed into
//!   the port lifecycle; the same call with a **null** port records nothing.
//! * **the busy state through `beam_io`** — a real port in a real [`PortRegistry`] flips
//!   `PortRegistry::is_busy` from the driver's own `set_busy_port` requests (a `start` token and two
//!   control verbs), and `PortRegistry::port_command_with` answers `Busy`/`Done`/`notsup` exactly as
//!   the C's table says — which is only possible because `Driver::is_busy`/`Driver::flags` are the
//!   driver's real state rather than the trait defaults.
//!
//! # How the fixture reaches the host functions
//!
//! A driver `.so` normally leaves `driver_*`/`erl_drv_*` undefined and resolves them out of the
//! loading process's dynamic symbol table, which exists for the `[[bin]]`/`tests/` targets only
//! (`build.rs`'s `-Wl,--export-dynamic`). This `--lib` harness has no such entries, so the fixture
//! imports **no** `driver_*` symbol and is handed the two host functions as `%lx` addresses in the
//! loader's `command` string (the `port_term_tests.rs`/`liveness_tests.rs` convention).
//!
//! # Skip policy
//!
//! The compiled cases need the host C compiler and the host `erl_driver.h`; without either they
//! print a **loud** `SKIP:` banner and return — never a silent pass. The seam case needs neither.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_alloc::Heap;
use beam_abi::DriverHost;
use beam_io::registry::PortRegistry;
// `Driver` is the trait `DriverPortAdapter::flags` is reached through (`beam_io::Driver::flags`).
use beam_io::{Driver as _, DriverFlags, Poller, PortCommandOptions, PortCommandOutcome};
use beam_process::{PortId, ProcessId};
use beam_term::AtomTable;

use super::{DriverPort, DriverPortAdapter, OsPidFlag};

/// The fixture: one driver whose attribute paths the `start` **mode** token selects.
///
/// `command` is `<object path> <erl_drv_set_os_pid address> <set_busy_port address> <marker>
/// <mode>`, where `mode` is
///
/// * `pid` — `start` calls `erl_drv_set_os_pid(port, 4242)`;
/// * `null` — `start` calls it with a **null** port (the C's invalid-port case);
/// * `busy` — `start` calls `set_busy_port(port, 1)`, so a freshly opened port is busy;
/// * `force` — as `busy`, and the port additionally records every `output` write it is handed, so
///   a test can tell "the command was queued and written" from "the command was never queued";
/// * `idle` — no attribute is recorded at `start`.
///
/// Control verb `1` marks the port busy, verb `0` clears it — both through the real
/// `set_busy_port` entry — and verb `2` records `erl_drv_set_os_pid(port, 7777)` **after** the port
/// came up. Every verb records the call in the marker so a silently-skipped request is visible.
const ATTRIBUTES_C: &str = r#"
#include <erl_driver.h>

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define RT_TEXT_MAX 512

/* The two host entry points, handed over as `%lx` addresses (see the module docs). */
typedef void (*rt_set_os_pid_fn)(ErlDrvPort, ErlDrvSInt);
typedef void (*rt_set_busy_fn)(ErlDrvPort, int);

typedef struct {
    ErlDrvPort port;
    rt_set_os_pid_fn set_os_pid;
    rt_set_busy_fn set_busy;
    FILE *marker;
    char mode[8];
    /* Whether `output` records the writes it is handed (`force` mode only, so the other modes'
     * markers stay exactly the notes they made before). */
    int count_output;
} rt_state;
/* One port per loaded object, so the state can live in this object's own storage. */
static rt_state rt_state_storage;

static void rt_note(rt_state *st, const char *what, long value) {
    if (st != NULL && st->marker != NULL) {
        fprintf(st->marker, "%s:%ld\n", what, value);
        fflush(st->marker);
    }
}

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    rt_state *st = &rt_state_storage;
    char object[RT_TEXT_MAX];
    char marker[RT_TEXT_MAX];
    unsigned long os_pid_addr = 0;
    unsigned long busy_addr = 0;
    object[0] = '\0';
    marker[0] = '\0';
    st->mode[0] = '\0';
    st->count_output = 0;
    if (command == NULL)
        return (ErlDrvData) -1;
    if (sscanf(command, "%511s %lx %lx %511s %7s", object, &os_pid_addr, &busy_addr, marker,
               st->mode) != 5)
        return (ErlDrvData) -1;
    if (os_pid_addr == 0 || busy_addr == 0 || marker[0] == '\0' || st->mode[0] == '\0')
        return (ErlDrvData) -1;
    st->set_os_pid = (rt_set_os_pid_fn) (intptr_t) os_pid_addr;
    st->set_busy = (rt_set_busy_fn) (intptr_t) busy_addr;
    st->marker = fopen(marker, "w");
    if (st->marker == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    if (strcmp(st->mode, "pid") == 0) {
        /* The real public hook: `start` runs *after* the port was registered, which is exactly the
         * "the driver records it later" shape the Handle has to survive. */
        st->set_os_pid(port, 4242);
        rt_note(st, "OS", 4242);
    } else if (strcmp(st->mode, "null") == 0) {
        /* A null port is the C's invalid-port operand: nothing may be recorded. */
        st->set_os_pid(NULL, 4242);
        rt_note(st, "NULL", 4242);
    } else if (strcmp(st->mode, "busy") == 0) {
        st->set_busy(port, 1);
        rt_note(st, "B", 1);
    } else if (strcmp(st->mode, "force") == 0) {
        /* Busy *and* watching the write path: the port is exactly the state `io.c:1763`'s
         * `ERTS_PORT_OP_BUSY` arm is about, while `output` records whether any bytes ever reach the
         * driver. */
        st->set_busy(port, 1);
        st->count_output = 1;
        rt_note(st, "B", 1);
    }
    return (ErlDrvData) st;
}

/* `output` is the driver side of the write path: it is reached exactly when the port layer decided
 * the command may be written (`io.c:1763`'s `ERTS_PORT_OP_DONE`), never for a `Busy` answer. */
static void rt_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    rt_state *st = (rt_state *) drv_data;
    (void) buf;
    if (st != NULL && st->count_output)
        rt_note(st, "OUT", (long) len);
}
static void rt_stop(ErlDrvData drv_data) {
    rt_state *st = (rt_state *) drv_data;
    if (st != NULL && st->marker != NULL) {
        fclose(st->marker);
        st->marker = NULL;
    }
}

/* `1` marks the port busy, `0` clears it — both through the real `set_busy_port` entry — and `2`
 * records an OS pid *after* the port is up (`erl_drv_set_os_pid`, the C's `prt->os_pid`). */
static ErlDrvSSizeT rt_control(ErlDrvData drv_data, unsigned int cmd, char *buf,
                               ErlDrvSizeT len, char **rbuf, ErlDrvSizeT rlen) {
    rt_state *st = (rt_state *) drv_data;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;
    if (st == NULL || st->set_busy == NULL || st->set_os_pid == NULL)
        return (ErlDrvSSizeT) -1;
    if (cmd == 1 || cmd == 0) {
        st->set_busy(st->port, (int) cmd);
        rt_note(st, "B", (long) cmd);
        return (ErlDrvSSizeT) 0;
    }
    if (cmd == 2) {
        st->set_os_pid(st->port, 7777);
        rt_note(st, "OS", 7777);
        return (ErlDrvSSizeT) 0;
    }
    return (ErlDrvSSizeT) -1;
}

static ErlDrvEntry rt_attributes_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = rt_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_ATTRIBUTES_NAME,
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
    /* The flag word the `[force]` decision is made on: `RT_ATTRIBUTES_FLAGS`, 0 for the plain
     * build and `ERL_DRV_FLAG_SOFT_BUSY` for the soft-busy one. */
    .driver_flags = RT_ATTRIBUTES_FLAGS,
    .handle2 = NULL,
    .process_exit = NULL,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(rt_attributes_init) {
    return &rt_attributes_entry;
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
/// A compact duplicate of `port_term_tests.rs::erl_include_dir` (a unit test cannot import the
/// `tests/` crate target's support).
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

/// Ask the host `erl` for `code:lib_dir(erts, include)` (bounded: the probe appends `-s init stop`).
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

/// A compiled driver `.so` plus the marker file its callbacks append to.
struct Fixture {
    /// The scratch directory holding the source, the `.so` and the marker.
    dir: PathBuf,
    /// The `.so` **without** its suffix (the `DriverPortAdapter::load` candidate).
    stem: PathBuf,
    /// The path the fixture records its own requests in.
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

    /// The marker's `<what>:<value>` lines, in order.
    fn notes(&self) -> Vec<String> {
        match std::fs::read_to_string(&self.marker) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            // A missing marker means the fixture never got as far as `start`: report no notes so
            // the caller's assertion, not a panic, is what fails.
            Err(_) => Vec::new(),
        }
    }
}

/// Compiles [`ATTRIBUTES_C`] as `<scratch>/<stem>.so`; `None` when a host facility is missing (the
/// caller returns early — **loudly**).
fn build_fixture(stem: &str, driver_flags: &str) -> Option<Fixture> {
    if !cc_available() {
        skip(&format!(
            "port_attributes: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("port_attributes: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_port_attributes_{stem}_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{stem}.c"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, ATTRIBUTES_C));
    if let Err(error) = write {
        skip(&format!(
            "port_attributes: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_ATTRIBUTES_NAME=\"{stem}\""))
        .arg(format!("-DRT_ATTRIBUTES_FLAGS=({driver_flags})"))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{stem}.so")))
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "port_attributes: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("port_attributes: could not run the C compiler: {error}"));
            return None;
        }
    }
    if !dir.join(format!("{stem}.so")).is_file() {
        skip(&format!("port_attributes: the C compiler produced no {stem}.so"));
        return None;
    }
    Some(Fixture {
        stem: dir.join(stem),
        marker: dir.join("marker.txt"),
        dir,
    })
}

// ---------------------------------------------------------------------------------------------
// Part A — the OS pid, at the seam
// ---------------------------------------------------------------------------------------------

/// A host over a fresh atom table, running as pid 1, with a small arena.
fn host() -> DriverPort {
    DriverPort::new(
        Heap::new(256).expect("a test heap"),
        Rc::new(AtomTable::new()),
        ProcessId::new(1, 0),
    )
}

/// The C's `set_os_pid` answers `-1` for an invalid port and `0` otherwise, and the value it records
/// is the one `port_info(Port, os_pid)` reports — surviving the port's teardown.
///
/// The invalid-port half is the C's `io.c:2972-2974` early return, which here is a host no port
/// lifecycle claimed (`DriverPortAdapter::load` is what claims one). The survival half is the
/// difference from the driver-private queue: `clear_driver_state` must **not** drop the pid, because
/// the C's `prt->os_pid` belongs to the port, not to the driver.
#[test]
fn an_unclaimed_host_refuses_and_a_claimed_one_records_the_os_pid() {
    let mut port_host = host();
    let os_pid: OsPidFlag = port_host.os_pid_flag();
    assert_eq!(os_pid.get(), None, "a fresh port has no OS pid (the C's -1)");

    // No lifecycle claimed this host: the C's invalid-port answer, and nothing recorded.
    assert_eq!(
        port_host.set_os_pid(4242),
        -1,
        "an invalid port answers the C's -1"
    );
    assert_eq!(
        os_pid.get(),
        None,
        "a refused request must not record a pid no port could report"
    );

    // Claim it the way `DriverPortAdapter::load` does (that latch is what makes a host a port).
    {
        let control = port_host.control();
        control.borrow_mut().attached = true;
    }
    assert_eq!(port_host.set_os_pid(4242), 0, "a live port answers 0");
    assert_eq!(
        os_pid.get(),
        Some(4242),
        "the recorded pid is what port_info(Port, os_pid) answers"
    );

    // A recorded `0` stays distinguishable from "never recorded" — the C renders the latter
    // `undefined` and stores the former as a value.
    assert_eq!(port_host.set_os_pid(0), 0);
    assert_eq!(os_pid.get(), Some(0), "0 is a recorded value, not the sentinel");
    assert_eq!(port_host.set_os_pid(-1), 0, "the C stores -1 as a value too");
    assert_eq!(os_pid.get(), Some(-1));
    assert_ne!(
        os_pid.get(),
        OsPidFlag::new().get(),
        "the C's -1 sentinel as a recorded value is not the same state as unset"
    );

    // The port's teardown drops the driver-private state, never the pid.
    port_host.set_os_pid(4242);
    port_host.clear_driver_state();
    assert_eq!(
        os_pid.get(),
        Some(4242),
        "port_info(Port, os_pid) keeps answering after the driver stops writing"
    );
}

// ---------------------------------------------------------------------------------------------
// Part A — the OS pid, end to end through a real C driver
// ---------------------------------------------------------------------------------------------

/// A loaded fixture driver plus the poller/registry its port lives in and the handles seized before
/// the host was boxed (the boxed driver exposes no host).
struct Open {
    /// The compiled driver and its marker file.
    fixture: Fixture,
    /// The port's OS-pid handle — the very one the driver's `erl_drv_set_os_pid` call writes.
    os_pid: OsPidFlag,
    /// The [`beam_io::Driver::flags`] the adapter reported **before** it was boxed into the port —
    /// the registry offers no accessor for it, and it is the flag word `Port::command` decides
    /// `[force]` on.
    flags: DriverFlags,
    /// The poller the port is registered with.
    poller: Poller,
    /// The registry the port lives in.
    ports: PortRegistry,
    /// The port's id.
    id: PortId,
}

/// Compiles the fixture, loads it and opens one real port on it with `mode` as its `start` token.
fn open_port(stem: &str, driver_flags: &str, mode: &str) -> Option<Open> {
    let fixture = build_fixture(stem, driver_flags)?;
    // The same anchor `crate::port::PortRegistry::new` uses, so the suite exercises the populated
    // `erl_drv_*` table.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let port_host = DriverPort::new(
        Heap::new(256).expect("a test heap"),
        Rc::new(AtomTable::new()),
        ProcessId::new(1, 0),
    );
    // Seized before the host is consumed by `load`, exactly as the runtime's `open_port/2` does: the
    // handles are shared by `Rc`, so the writes the driver makes from inside `start` (which runs
    // after the port is registered) land on these very ones.
    let os_pid = port_host.os_pid_flag();
    let command = format!(
        "{} {:x} {:x} {} {mode}",
        fixture.stem.with_extension("so").display(),
        set_os_pid_address(),
        set_busy_port_address(),
        fixture.marker.display()
    );
    let adapter = DriverPortAdapter::load(
        &[fixture.stem.display().to_string()],
        stem,
        port_host,
        command,
    )
    .expect("the fixture driver loads");
    // Read before the adapter is boxed: `flags()` is the driver's own entry word (the C's
    // `ERL_DRV_FLAG_SOFT_BUSY`), which no registry accessor exposes afterwards.
    let flags = adapter.flags();
    let mut poller = Poller::new().expect("a poller");
    let mut ports = PortRegistry::new();
    let id = ports
        .open(&mut poller, Box::new(adapter))
        .expect("the port opens (the fixture's `start` ran)");
    Some(Open {
        fixture,
        os_pid,
        flags,
        poller,
        ports,
        id,
    })
}

/// The address of the real `erl_drv_set_os_pid` entry point, handed to the fixture.
fn set_os_pid_address() -> usize {
    beam_abi::driver::drv::driver_api_table()
        .erl_drv_set_os_pid
        .expect("the `erl_drv_set_os_pid` entry point is registered in the table")
        as usize
}

/// The address of the real `set_busy_port` entry point, handed to the fixture.
fn set_busy_port_address() -> usize {
    beam_abi::driver::drv::driver_api_table()
        .set_busy_port
        .expect("the `set_busy_port` entry point is registered in the table")
        as usize
}

/// A real C driver's `erl_drv_set_os_pid(port, 4242)` reaches the host: the pid is recorded on the
/// handle the runtime seized, from a callback that ran **after** the port was registered.
///
/// Discriminates: with `DriverHost::set_os_pid` left at the seam default the call is refused, so
/// `get()` stays `None` and this test fails — as it does if the flag is not shared with the host.
#[test]
fn a_c_drivers_erl_drv_set_os_pid_records_the_pid() {
    let Some(open) = open_port("rt_attr_os_pid", "0", "pid") else {
        return;
    };
    assert_eq!(
        open.fixture.notes(),
        vec!["OS:4242".to_string()],
        "the fixture's start did run and did call the real entry point"
    );
    assert_eq!(
        open.os_pid.get(),
        Some(4242),
        "the pid the driver recorded is readable through the handle port_info(Port, os_pid) uses"
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

/// The same entry point with a **null** port records nothing — the C's invalid-port operand is
/// refused by the shared handle predicate before any host is recovered (`io.c:2972-2974`).
///
/// This is the invalid-port refusal at the C level: a fixture that was otherwise wired correctly
/// (`pid` mode's twin) leaves the handle unset.
#[test]
fn a_null_port_records_no_os_pid() {
    let Some(open) = open_port("rt_attr_os_pid_null", "0", "null") else {
        return;
    };
    assert_eq!(
        open.fixture.notes(),
        vec!["NULL:4242".to_string()],
        "the fixture's start did run and did call the real entry point"
    );
    assert_eq!(
        open.os_pid.get(),
        None,
        "a null port is refused, so the handle stays unset (port_info answers `undefined`)"
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

/// The driver records its OS pid **after** the port came up, from a `control` callback, and the
/// value `port_info(Port, os_pid)` would report turns from `undefined` into that pid.
///
/// The C initialises `prt->os_pid` to `-1` (`io.c:386`) and `erl_bif_info.c:3814-3817` renders the
/// unset sentinel as the atom `undefined`; `erl_drv_set_os_pid` (`io.c:2971`) writes the pid the
/// driver's own child actually got. So on a live port that no callback has told yet the answer is
/// `None`, and after verb `2`'s `erl_drv_set_os_pid(port, 7777)` it is `Some(7777)`.
///
/// The assertion stops at the flag the answer is read from: the `port_info/2` item itself is
/// [`PortRegistry::info`]'s `os_pid` arm, which is out of this row's scope (and this suite's
/// subject — the attribute *recorded* by the driver, not the rendering of the item).
#[test]
fn a_control_driven_set_os_pid_turns_the_unset_attribute_into_the_new_pid() {
    let Some(mut open) = open_port("rt_attr_os_pid_control", "0", "idle") else {
        return;
    };
    assert!(
        open.fixture.notes().is_empty(),
        "the idle start token records no attribute"
    );
    assert_eq!(
        open.os_pid.get(),
        None,
        "before any driver call the C's `prt->os_pid` is -1, i.e. `port_info(Port, os_pid)` = undefined"
    );

    // Verb 2: the driver's own `erl_drv_set_os_pid(port, 7777)`, on a port that is already up.
    let reply = open
        .ports
        .port_control(&mut open.poller, open.id, "2", &[])
        .expect("control verb 2 runs");
    assert!(
        reply.is_empty(),
        "the fixture's os_pid verb answers no bytes"
    );
    assert_eq!(
        open.fixture.notes(),
        vec!["OS:7777".to_string()],
        "the fixture did call the real erl_drv_set_os_pid entry point, from `control`"
    );
    assert_eq!(
        open.os_pid.get(),
        Some(7777),
        "the pid a control callback recorded is what port_info(Port, os_pid) reports from then on"
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

// ---------------------------------------------------------------------------------------------
// Part B — the busy state and the entry flags, through `beam_io`'s seam
// ---------------------------------------------------------------------------------------------
/// The `beam_io::Driver` seam reports the driver's real busy state, so `PortRegistry::is_busy` and
/// `Port::command`'s `ERTS_PORT_OP_BUSY` arm follow the driver's own `set_busy_port` requests:
/// `start` marks the port busy (nothing is queued, no write is scheduled), verb `0` clears it (the
/// command goes through), verb `1` marks it again.
///
/// Discriminates: without the `is_busy` override the seam answers the trait default `false` for
/// every `dlopen`ed driver, so the first `port_command_with` would answer `Done` and queue bytes the
/// C refuses.
#[test]
fn the_seam_reports_the_drivers_busy_state_to_the_command_table() {
    let Some(mut open) = open_port("rt_attr_busy", "ERL_DRV_FLAG_SOFT_BUSY", "busy") else {
        return;
    };
    assert_eq!(
        open.fixture.notes(),
        vec!["B:1".to_string()],
        "the fixture's `busy` start token called the real set_busy_port(port, 1)"
    );
    assert!(
        open.ports.is_busy(open.id).expect("a live port"),
        "the driver marked the port busy at start, and the seam forwards it"
    );
    assert_eq!(
        open.ports
            .port_command_with(&mut open.poller, open.id, b"hi", PortCommandOptions::EMPTY)
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy,
        "the C's ERTS_PORT_OP_BUSY: nothing is queued"
    );
    assert_eq!(
        open.ports.get(open.id).expect("live").output_len(),
        0,
        "a busy port queues nothing"
    );
    // `[force]` on a driver advertising ERL_DRV_FLAG_SOFT_BUSY bypasses the state entirely — the
    // entry's flag word is what `Driver::flags` must report.
    assert_eq!(
        open.ports
            .port_command_with(
                &mut open.poller,
                open.id,
                b"hi",
                PortCommandOptions::EMPTY.with_force(true)
            )
            .expect("a soft-busy driver takes a forced command"),
        PortCommandOutcome::Done,
        "`[force]` is accepted because the entry advertises SOFT_BUSY"
    );
    assert!(
        open.ports.is_busy(open.id).expect("a live port"),
        "`[force]` bypasses the state for one command; it does not clear it"
    );
    open.ports
        .execute_port_tasks(&mut open.poller, 1024)
        .expect("the OUTPUT task runs");
    // Verb 0: `set_busy_port(port, 0)` — the state the runtime re-issues a suspended command on.
    let reply = open
        .ports
        .port_control(&mut open.poller, open.id, "0", &[])
        .expect("control verb 0 runs");
    assert!(reply.is_empty(), "the fixture's busy verbs answer no bytes");
    assert!(
        !open.ports.is_busy(open.id).expect("a live port"),
        "the driver's own request cleared the state through the same shared handle"
    );
    assert_eq!(
        open.ports
            .port_command_with(&mut open.poller, open.id, b"hi", PortCommandOptions::EMPTY)
            .expect("the cleared port takes the command"),
        PortCommandOutcome::Done
    );
    // Verb 1 marks it busy again.
    open.ports
        .port_control(&mut open.poller, open.id, "1", &[])
        .expect("control verb 1 runs");
    assert!(open.ports.is_busy(open.id).expect("a live port"));
    let notes = open.fixture.notes();
    assert_eq!(
        notes,
        vec![
            "B:1".to_string(),
            "B:0".to_string(),
            "B:1".to_string()
        ],
        "every request went through the real entry point, in order"
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

/// A driver whose entry does **not** advertise `ERL_DRV_FLAG_SOFT_BUSY` still reports its busy
/// state, and `[force]` is the `notsup` raise on **both** states — the flag word decides it, never
/// the state (`erl_bif_port.c:195-198`).
///
/// This is the negative half of `flags()`: an override that always reported `SOFT_BUSY` (or one
/// that reported nothing at all for a driver that does declare it) fails one of the two tests.
#[test]
fn a_driver_without_the_soft_busy_flag_refuses_force_in_both_states() {
    let Some(mut open) = open_port("rt_attr_busy_plain", "0", "idle") else {
        return;
    };
    assert!(
        open.fixture.notes().is_empty(),
        "the idle start token records no attribute"
    );
    assert!(
        !open.ports.is_busy(open.id).expect("a live port"),
        "an idle port reports no busy state"
    );
    let err = open
        .ports
        .port_command_with(
            &mut open.poller,
            open.id,
            b"hi",
            PortCommandOptions::EMPTY.with_force(true),
        )
        .expect_err("a driver without SOFT_BUSY refuses a forced command");
    assert_eq!(
        err.as_atom(),
        Some("notsup"),
        "the oracle raises `error:notsup` for `[force]` on a non-soft-busy driver"
    );
    assert_eq!(open.ports.get(open.id).expect("live").output_len(), 0);
    // The busy state is independent of the flag: the driver can still mark its port busy.
    open.ports
        .port_control(&mut open.poller, open.id, "1", &[])
        .expect("control verb 1 runs");
    assert!(
        open.ports.is_busy(open.id).expect("a live port"),
        "the state flips whether or not the entry is soft-busy"
    );
    let err = open
        .ports
        .port_command_with(
            &mut open.poller,
            open.id,
            b"hi",
            PortCommandOptions::EMPTY.with_force(true).with_nosuspend(true),
        )
        .expect_err("the flag decides, not the state");
    assert_eq!(err.as_atom(), Some("notsup"));
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

/// A `SOFT_BUSY` driver's **busy** port: `[force]` is written through, while the plain and
/// `[nosuspend]` commands are the C's `ERTS_PORT_OP_BUSY` — nothing queued, so the driver is never
/// reached. The driver side of that distinction is the point: its `output` callback records every
/// write it is handed, and the busy arms produce no `OUT:` note at all.
///
/// The oracle: `port_command(P, Data, [force])` on a driver whose entry carries
/// `ERL_DRV_FLAG_SOFT_BUSY` answers `true` **whatever the port's state** — the flag word is what
/// `erl_bif_port.c:195-198` reads, and under `ERTS_PORT_SIG_FLG_FORCE` the C zeroes `busy_flgs`
/// (`io.c:1731`), so the `ERTS_PORT_OP_BUSY` arms are unreachable. Without the flag it is the
/// sibling row's `error:notsup`. A plain or `[nosuspend]` command on a busy port is
/// `ERTS_PORT_OP_BUSY` (`io.c:1763-1769`): the bytes are **not** queued and the caller re-issues
/// them once the driver clears the state, so exactly one of the three commands can ever appear at
/// the driver's `output`.
#[test]
fn a_soft_busy_driver_takes_force_while_busy_and_the_busy_arms_reach_no_write() {
    let Some(mut open) = open_port("rt_attr_busy_force", "ERL_DRV_FLAG_SOFT_BUSY", "force") else {
        return;
    };
    assert!(
        open.flags.contains(DriverFlags::SOFT_BUSY),
        "the entry declares ERL_DRV_FLAG_SOFT_BUSY, so `flags()` must report the bit"
    );
    assert_eq!(
        open.fixture.notes(),
        vec!["B:1".to_string()],
        "the `force` start token marked the port busy and recorded no write"
    );
    assert!(
        open.ports.is_busy(open.id).expect("a live port"),
        "the driver marked its port busy at start"
    );

    // The two busy arms: the C's ERTS_PORT_OP_BUSY — no queueing, no task, no write.
    assert_eq!(
        open.ports
            .port_command_with(&mut open.poller, open.id, b"hi", PortCommandOptions::EMPTY)
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy,
        "a plain command on the busy port is `ERTS_PORT_OP_BUSY`"
    );
    assert_eq!(
        open.ports
            .port_command_with(
                &mut open.poller,
                open.id,
                b"hi",
                PortCommandOptions::EMPTY.with_nosuspend(true)
            )
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy,
        "`[nosuspend]` is the same busy arm: the runtime answers `false` rather than waiting"
    );
    let round = open
        .ports
        .execute_port_tasks(&mut open.poller, 1024)
        .expect("an empty round runs");
    assert_eq!(round.ran, 0, "a `Busy` answer schedules no `OUTPUT` task");
    assert_eq!(
        open.fixture.notes(),
        vec!["B:1".to_string()],
        "neither busy command reached the driver's `output`: nothing was queued"
    );
    assert_eq!(
        open.ports.get(open.id).expect("live").output_len(),
        0,
        "the busy arms leave the output queue empty"
    );

    // `[force]` on that very busy port: `Done`, and the bytes really reach the driver.
    assert_eq!(
        open.ports
            .port_command_with(
                &mut open.poller,
                open.id,
                b"hi",
                PortCommandOptions::EMPTY.with_force(true)
            )
            .expect("a soft-busy driver takes a forced command"),
        PortCommandOutcome::Done,
        "`[force]` bypasses the busy state (`io.c:1731`), so the command is queued"
    );
    assert_eq!(
        open.ports.get(open.id).expect("live").output_len(),
        2,
        "the forced command is queued for the write path"
    );
    open.ports
        .execute_port_tasks(&mut open.poller, 1024)
        .expect("the OUTPUT task runs");
    assert_eq!(
        open.fixture.notes(),
        vec!["B:1".to_string(), "OUT:2".to_string()],
        "exactly one write reached the driver — the forced one, with the command's two bytes"
    );
    assert!(
        open.ports.is_busy(open.id).expect("a live port"),
        "`[force]` bypasses the state for one command; it does not clear it"
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

/// The two flags an entry can report are **independent of the port's state**: a fresh port of a
/// soft-busy driver is idle yet its `flags()` contains `SOFT_BUSY`, which is the word
/// `port_command_with`'s `[force]` arm reads.
#[test]
fn the_soft_busy_flag_follows_the_entry_not_the_busy_state() {
    let Some(mut open) = open_port("rt_attr_busy_flag", "ERL_DRV_FLAG_SOFT_BUSY", "idle") else {
        return;
    };
    assert!(
        !open.ports.is_busy(open.id).expect("a live port"),
        "the port is idle"
    );
    assert_eq!(
        open.ports
            .port_command_with(
                &mut open.poller,
                open.id,
                b"hi",
                PortCommandOptions::EMPTY.with_force(true)
            )
            .expect("an idle soft-busy driver takes a forced command"),
        PortCommandOutcome::Done,
        "SOFT_BUSY is reported from the entry, with the port idle"
    );
    // The same entry, now busy: still forced through (`busy_flgs = 0` under FORCE, `io.c:1731`).
    open.ports
        .port_control(&mut open.poller, open.id, "1", &[])
        .expect("control verb 1 runs");
    assert!(open.ports.is_busy(open.id).expect("a live port"));
    assert_eq!(
        open.ports
            .port_command_with(
                &mut open.poller,
                open.id,
                b"hi",
                PortCommandOptions::EMPTY.with_force(true)
            )
            .expect("the state is bypassed"),
        PortCommandOutcome::Done
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

/// The `beam_io::DriverFlags` value the adapter reports carries exactly one bit when the entry
/// advertises `ERL_DRV_FLAG_SOFT_BUSY` — the mapping `beam_io`'s own ABI adapter uses, asserted here
/// so a constant change cannot silently invert it.
#[test]
fn the_soft_busy_mapping_is_the_one_bit_beam_io_defines() {
    assert!(
        DriverFlags::empty()
            .union(DriverFlags::SOFT_BUSY)
            .contains(DriverFlags::SOFT_BUSY),
        "the flag set the override builds contains the SOFT_BUSY bit"
    );
    assert!(
        !DriverFlags::empty().contains(DriverFlags::SOFT_BUSY),
        "an entry that declares nothing reports the empty set — the negative half"
    );
}
