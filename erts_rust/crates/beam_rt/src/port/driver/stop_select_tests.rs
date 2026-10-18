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

//! The A.4 **`ERL_DRV_USE` / `stop_select` regression test** for
//! [`DriverPortAdapter::apply_select`](super::DriverPortAdapter::apply_select).
//!
//! `driver_select(port, event, ERL_DRV_USE, 0)` is a driver's "I am done with this event" form:
//! the C clears the event's remaining directions and, once nothing is armed, hands the event back
//! through the entry's `stop_select` callback — the driver's licence to close the descriptor
//! (`erl_check_io.c:1177-1193`, `:1212-1216`). The pair test is deliberate: the *cancel* bit in
//! `ERL_DRV_USE_NO_CALLBACK` suppresses the callback, and a plain direction deselect must never
//! run it, so three payloads drive the three forms and each asserts its own outcome.
//!
//! It is a sibling of `ready_output_tests.rs` (a child of `../driver.rs`) for the same reason: the
//! C fixture text does not belong in that file.
//!
//! # What the fixture observes
//!
//! `stop_select` receives an `ErlDrvEvent` and **no** `ErlDrvData`, so it cannot reach the port's
//! private state through its arguments. The fixture therefore records two things in a **marker
//! file** whose path the test hands it in the loader's `command` string: the descriptor `start`
//! armed (`A:<fd>`), and the event the callback was handed (`S:<event>`). One file makes both
//! facts — and their identity — observable without a second port or a second load.
//!
//! `stop_select` also calls the host's `driver_output`, so the callback firing is visible a second
//! way, through the port's own input queue (what the owner receives as `{Port, {data, Data}}`).
//!
//! # How the fixture reaches the host (`driver_select`/`driver_output`)
//!
//! The same constraint `ready_output_tests.rs` documents: `--lib` unit-test links are not given
//! `-Wl,--export-dynamic`, so the fixture imports no `driver_*` symbol and is handed the two host
//! functions it needs as addresses in the `command` string.
//!
//! # Skip policy
//!
//! Needs the host C compiler and the host `erl_driver.h`; without either it prints a **loud**
//! `SKIP:` banner and returns — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_alloc::Heap;
use beam_io::registry::PortRegistry;
use beam_io::{Poller, PortCommandOptions};
use beam_process::{PortId, ProcessId};
use beam_term::AtomTable;

use super::{DriverPort, DriverPortAdapter};

/// The fixture: a driver that arms a descriptor with `ERL_DRV_USE` and, on command, deselects it
/// in one of the three forms (see the module docs).
///
/// `output` is the only entry point the test can reach, so it carries the command: `"U"` deselects
/// with `ERL_DRV_USE` (the "done with this event" form), `"P"` with a plain `ERL_DRV_WRITE`, and
/// `"C"` with `ERL_DRV_USE_NO_CALLBACK` (the cancel form). The marker file makes both the
/// descriptor and the `stop_select` event observable.
const STOP_SELECT_C: &str = r#"
#include <erl_driver.h>

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

/* The host's `driver_select`/`driver_output`, handed over in the `command` string (two `%lx`
 * addresses) together with the marker-file path (one `%s`). See the module docs. */
typedef int (*rt_select_fn)(ErlDrvPort, ErlDrvEvent, int, int);
typedef int (*rt_output_fn)(ErlDrvPort, char *, ErlDrvSizeT);

#define RT_MARKER_MAX 512

typedef struct {
    ErlDrvPort port;
    int read_end;
    int write_end;
    rt_select_fn select;
    rt_output_fn output;
} rt_state;

/* One port per loaded object, so the state can live in this object's own storage: the fixture
 * deliberately imports neither `driver_alloc`/`driver_free` nor any `driver_*` symbol. */
static rt_state rt_state_storage;

/* `stop_select` gets no `ErlDrvData`, so the marker is reached through this descriptor instead. */
static int rt_marker_fd = -1;

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    int fds[2];
    rt_state *st = &rt_state_storage;
    unsigned long select_addr = 0;
    unsigned long output_addr = 0;
    char marker[RT_MARKER_MAX];
    if (command == NULL)
        return (ErlDrvData) -1;
    marker[0] = '\0';
    if (sscanf(command, "%lx %lx %511s", &select_addr, &output_addr, marker) != 3)
        return (ErlDrvData) -1;
    if (select_addr == 0 || output_addr == 0 || marker[0] == '\0')
        return (ErlDrvData) -1;
    st->select = (rt_select_fn) (intptr_t) select_addr;
    st->output = (rt_output_fn) (intptr_t) output_addr;
    rt_marker_fd = open(marker, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (rt_marker_fd < 0)
        return (ErlDrvData) -1;
    if (pipe(fds) != 0)
        return (ErlDrvData) -1;
    st->port = port;
    st->read_end = fds[0];
    st->write_end = fds[1];
    /* Arm the write end **with** `ERL_DRV_USE`: that is the obligation a later `ERL_DRV_USE`
     * deselect discharges, and what makes the "last direction went away" branch the interesting
     * one. A refusal fails the port, so a half-armed port never looks healthy. */
    if (st->select(port, (ErlDrvEvent) (intptr_t) st->write_end,
                   ERL_DRV_WRITE | ERL_DRV_USE, 1) != 0) {
        close(fds[0]);
        close(fds[1]);
        return (ErlDrvData) -1;
    }
    dprintf(rt_marker_fd, "A:%d\n", st->write_end);
    return (ErlDrvData) st;
}

static void rt_stop(ErlDrvData drv_data) {
    /* The pipe's ends are deliberately left open: a port that still has a descriptor armed
     * deregisters it *after* `stop` has run, so closing the write end here would make that
     * deregistration fail (`EBADF`). The two fds each test leaks are reclaimed with the process. */
    (void) drv_data;
}

/* The form of the deselect the driver issues is chosen by the payload, so one entry covers all
 * three `driver_select` modes the test discriminates. */
static void rt_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    rt_state *st = (rt_state *) drv_data;
    if (st == NULL || len == 0)
        return;
    if (len == 1 && buf[0] == 'U')
        (void) st->select(st->port, (ErlDrvEvent) (intptr_t) st->write_end,
                          ERL_DRV_WRITE | ERL_DRV_USE, 0);
    else if (len == 1 && buf[0] == 'P')
        (void) st->select(st->port, (ErlDrvEvent) (intptr_t) st->write_end,
                          ERL_DRV_WRITE, 0);
    else if (len == 1 && buf[0] == 'C')
        (void) st->select(st->port, (ErlDrvEvent) (intptr_t) st->write_end,
                          ERL_DRV_WRITE | ERL_DRV_USE_NO_CALLBACK, 0);
}

static void rt_stop_select(ErlDrvEvent event, void *reserved) {
    rt_state *st = &rt_state_storage;
    (void) reserved;
    if (rt_marker_fd >= 0)
        dprintf(rt_marker_fd, "S:%ld\n", (long) (intptr_t) event);
    /* A second, independent observable: the callback firing lands bytes on the port's input
     * queue, exactly as any other `driver_output` would. */
    if (st->port != NULL && st->output != NULL)
        (void) st->output(st->port, "S", 1);
}

static ErlDrvEntry rt_stop_select_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = rt_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_STOP_SELECT_NAME,
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
    .process_exit = NULL,
    .stop_select = rt_stop_select,
    .emergency_close = NULL,
};

DRIVER_INIT(rt_stop_select_init) {
    return &rt_stop_select_entry;
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
    from_erl_eval()
        .or_else(from_path_erl)
        .or_else(|| {
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

/// A compiled driver `.so` plus the marker file its `stop_select` writes to.
struct Fixture {
    /// The scratch directory holding the source, the `.so` and the marker.
    dir: PathBuf,
    /// The `.so` **without** its suffix (the `DriverPortAdapter::load` candidate).
    stem: PathBuf,
    /// The path the fixture records the armed descriptor and the `stop_select` event in.
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

    /// The `A:<fd>` / `S:<event>` lines the fixture has written so far.
    ///
    /// Read after the callback has returned — the fixture `dprintf`s each line as it happens, so no
    /// flush is needed.
    fn marker_lines(&self) -> Vec<(char, i64)> {
        let text = match std::fs::read_to_string(&self.marker) {
            Ok(text) => text,
            // A missing marker means the fixture never got as far as `start`: report it as no
            // lines so the caller's assertion, not a panic, is what fails.
            Err(_) => return Vec::new(),
        };
        text.lines()
            .filter_map(|line| {
                let (tag, value) = line.split_once(':')?;
                let tag = tag.chars().next()?;
                let value = value.trim().parse::<i64>().ok()?;
                Some((tag, value))
            })
            .collect()
    }

    /// The single value recorded under `tag`, or `None`.
    fn marker_value(&self, tag: char) -> Option<i64> {
        self.marker_lines()
            .into_iter()
            .find(|(candidate, _)| *candidate == tag)
            .map(|(_, value)| value)
    }
}

/// Compiles [`STOP_SELECT_C`] as `<scratch>/<stem>.so`; `None` when a host facility is missing
/// (the caller returns early — **loudly**).
fn build_fixture(stem: &str) -> Option<Fixture> {
    if !cc_available() {
        skip(&format!("stop_select: no C compiler ({} / $CC) available", cc()));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("stop_select: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_stop_select_{stem}_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{stem}.c"));
    let write =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, STOP_SELECT_C));
    if let Err(error) = write {
        skip(&format!(
            "stop_select: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    // `cc -shared -fPIC -DRT_STOP_SELECT_NAME="<stem>" -I<include> -o <dir>/<stem>.so <dir>/<stem>.c`
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_STOP_SELECT_NAME=\"{stem}\""))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{stem}.so")))
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "stop_select: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("stop_select: could not run the C compiler: {error}"));
            return None;
        }
    }
    if !dir.join(format!("{stem}.so")).is_file() {
        skip(&format!(
            "stop_select: the C compiler produced no {stem}.so"
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

/// Loads `fixture`'s driver and opens one port on it.
///
/// The fixture is handed this runtime's `driver_select`/`driver_output` as two `%lx` addresses and
/// the marker path, all in the `command` string (see the module docs).
fn open_port(fixture: &Fixture) -> (Poller, PortRegistry, PortId) {
    // The same anchor `crate::port::PortRegistry::new` uses.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let atoms = Rc::new(AtomTable::new());
    let host = DriverPort::new(Heap::new(256).expect("a test heap"), atoms, ProcessId::new(1, 0));
    let command = format!(
        "{:x} {:x} {}",
        beam_abi::driver::drv::driver_select as *const () as usize,
        beam_abi::driver::drv::driver_output as *const () as usize,
        fixture.marker.display()
    );
    let adapter = DriverPortAdapter::load(
        &[fixture.stem.display().to_string()],
        &fixture.name,
        host,
        command,
    )
    .expect("the fixture driver loads");
    let mut poller = Poller::new().expect("a poller");
    let mut ports = PortRegistry::new();
    let id = ports
        .open(&mut poller, Box::new(adapter))
        .expect("the port opens (the fixture's `start` armed a writable descriptor)");
    (poller, ports, id)
}

// ---------------------------------------------------------------------------------------------
// The discriminating cases
// ---------------------------------------------------------------------------------------------

/// Deselecting with `ERL_DRV_USE` runs the driver's `stop_select` with **the event the driver
/// selected** — the C's "done with this event" form (`erl_check_io.c:1177-1193`, `:1212-1216`).
///
/// Discriminates: before the fix the last-direction-went-away branch deregistered the event and
/// stopped, so the marker carried only the `A:` line and the port's input queue was empty — both
/// assertions fail on that shape.
#[test]
fn a_use_deselect_runs_stop_select_with_the_selected_event() {
    let Some(fixture) = build_fixture("rt_stop_select_use") else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);
    let armed = fixture
        .marker_value('A')
        .expect("`start` records the descriptor it armed");
    assert!(armed > 0, "a real descriptor: {armed}");

    ports
        .port_command(&mut poller, id, b"U", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    let outcome = ports
        .execute_port_tasks(&mut poller, 1024)
        .expect("the port-task round runs");
    assert!(outcome.ran >= 1, "the OUTPUT task ran: {outcome:?}");

    // The callback ran, and it was handed the event the driver selected — not an fd the runtime
    // invented and not a null event.
    assert_eq!(
        fixture.marker_value('S'),
        Some(armed),
        "`stop_select` must receive the very event `driver_select` armed"
    );
    // ...and its `driver_output` reached the owner's side of the port.
    let mut data = Vec::new();
    for notification in &outcome.notifications {
        if let beam_io::PortNotification::Data { port, read } = notification {
            if *port == id {
                data.extend_from_slice(&read.flattened());
            }
        }
    }
    assert_eq!(data, b"S", "the callback's output reached the port: {outcome:?}");

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// A plain direction deselect must **not** run `stop_select`: the C tests the pair
/// `(mode & ERL_DRV_USE_NO_CALLBACK) == ERL_DRV_USE` (`erl_check_io.c:1177`), so a deselect that
/// names only `ERL_DRV_WRITE` leaves the callback owed.
#[test]
fn a_plain_deselect_does_not_run_stop_select() {
    let Some(fixture) = build_fixture("rt_stop_select_plain") else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);
    assert!(fixture.marker_value('A').is_some(), "the descriptor was armed");

    ports
        .port_command(&mut poller, id, b"P", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    let outcome = ports
        .execute_port_tasks(&mut poller, 1024)
        .expect("the port-task round runs");
    assert!(outcome.ran >= 1, "the OUTPUT task ran: {outcome:?}");

    assert_eq!(
        fixture.marker_value('S'),
        None,
        "a plain deselect must not run `stop_select`"
    );
    for notification in &outcome.notifications {
        if let beam_io::PortNotification::Data { port, read } = notification {
            assert!(
                *port != id || read.flattened().is_empty(),
                "no callback output: {read:?}"
            );
        }
    }

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// `ERL_DRV_USE_NO_CALLBACK` deselects **without** the callback — the C's cancel form
/// (`ERL_DRV_USE_NO_CALLBACK = ERL_DRV_USE | (ERL_DRV_USE << 1)`, `erl_driver.h:64`, whose high bit
/// makes the pair test fail).
#[test]
fn a_cancel_deselect_does_not_run_stop_select() {
    let Some(fixture) = build_fixture("rt_stop_select_cancel") else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);
    assert!(fixture.marker_value('A').is_some(), "the descriptor was armed");

    ports
        .port_command(&mut poller, id, b"C", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    let outcome = ports
        .execute_port_tasks(&mut poller, 1024)
        .expect("the port-task round runs");
    assert!(outcome.ran >= 1, "the OUTPUT task ran: {outcome:?}");

    assert_eq!(
        fixture.marker_value('S'),
        None,
        "the cancel form must not run `stop_select`"
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}
