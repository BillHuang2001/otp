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

//! The A.4 **write-path regression test** for [`DriverPortAdapter::ready_output`].
//!
//! A `port_command/2` queues bytes on the port's output queue and schedules an `OUTPUT` port task.
//! The task reaches [`DriverPortAdapter::ready_output`], which must put those bytes on this port's
//! **single write path** ([`DriverPortAdapter::output`] → the entry's `outputv`/`output`, the C's
//! `io.c:1782`) *and* run the entry's `ready_output` slot as the write-**readiness** notification
//! (the C's `ERTS_PORT_TASK_OUTPUT` arm, `erl_port_task.c:1793-1797`). The discriminating case is a
//! driver that armed a **writable** descriptor with `driver_select` in `start`: if the notification
//! displaced the write, the queued command would never be handed to the driver.
//!
//! It is a child of `../driver.rs` rather than part of its `#[cfg(test)] mod tests` so the C fixture
//! text it needs does not grow that file by another ~250 lines.
//!
//! # Why a real C `.so`
//!
//! Only a real driver can arm a *writable* descriptor and only a real driver's `output` callback can
//! prove the bytes arrived, so the fixture is compiled with the host `cc` against the host
//! `erl_driver.h` and `dlopen`ed exactly as `beam_rt`'s `{spawn_driver, Name}` port does. Two entry
//! shapes come from one source via the `RT_READY_OUTPUT_MODE` macro (the `event_driver.c`
//! convention): mode 0 leaves `ready_output` `NULL` (the pure case — the C substitutes
//! `no_ready_output_callback`, which logs *and* deselects), mode 1 declares it, so the order the C
//! uses (payload first, notification second) is pinned too.
//!
//! # Skip policy
//!
//! The test needs the host C compiler and the host `erl_driver.h`; when either is missing it prints
//! a **loud** `SKIP:` banner and returns — never a silent pass (the `tests/driver.rs` policy).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_alloc::Heap;
use beam_io::registry::PortRegistry;
use beam_io::{Poller, PortCommandOptions, PortNotification};
use beam_process::{PortId, ProcessId};
use beam_term::AtomTable;

use super::{DriverPort, DriverPortAdapter};

/// The fixture, in two compile-time modes (see the module docs).
///
/// `start` opens a `pipe(2)` and arms the **write** end with `driver_select`, exactly the shape that
/// reaches the buggy path. `output` echoes the payload back through `driver_output`, so the echo is
/// direct evidence that the callback ran *and* saw the bytes (it lands on the port's input queue,
/// which is what the owner observes). Mode 1's `ready_output` appends a `"W"` marker, so the
/// observed byte order pins the C's payload-then-notification order.
///
/// The mode's `driver_name` must equal the stem the library is compiled and loaded under (the loader
/// rejects a mismatch with `bad_driver_name`), so the two names are fixed here.
const READY_OUTPUT_C: &str = r#"
#include <erl_driver.h>

#include <stdint.h>
#include <unistd.h>

#ifndef RT_READY_OUTPUT_MODE
#define RT_READY_OUTPUT_MODE 0
#endif

#if RT_READY_OUTPUT_MODE == 1
#define RT_READY_OUTPUT_NAME "rt_ready_output_notify"
#else
#define RT_READY_OUTPUT_NAME "rt_ready_output"
#endif

#define RT_READY_OUTPUT_INIT rt_ready_output

typedef struct {
    ErlDrvPort port;
    int read_end;
    int write_end;
} rt_state;

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    int fds[2];
    rt_state *st;
    (void) command;
    if (pipe(fds) != 0)
        return (ErlDrvData) -1;
    st = (rt_state *) driver_alloc(sizeof(rt_state));
    if (st == NULL) {
        close(fds[0]);
        close(fds[1]);
        return (ErlDrvData) -1;
    }
    st->port = port;
    st->read_end = fds[0];
    st->write_end = fds[1];
    /* Arm the descriptor for writability BEFORE anything is queued: that is the state
     * `ready_output` has to cope with. A refusal fails the port, so a half-armed port never looks
     * healthy. */
    if (driver_select(port, (ErlDrvEvent) (intptr_t) st->write_end, ERL_DRV_WRITE, 1) != 0) {
        driver_free(st);
        close(fds[0]);
        close(fds[1]);
        return (ErlDrvData) -1;
    }
    return (ErlDrvData) st;
}

static void rt_stop(ErlDrvData drv_data) {
    rt_state *st = (rt_state *) drv_data;
    if (st == NULL)
        return;
    close(st->read_end);
    close(st->write_end);
    driver_free(st);
}

/* `port_command` reaches this callback. Echoing the payload back makes the arrival of the bytes
 * observable: only the write path invokes `output`, and only with the queued bytes. */
static void rt_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    rt_state *st = (rt_state *) drv_data;
    if (st == NULL || len == 0)
        return;
    (void) driver_output(st->port, buf, len);
}

#if RT_READY_OUTPUT_MODE == 1
/* The write-readiness notification: a one-byte marker, emitted AFTER the payload. */
static void rt_ready_output(ErlDrvData drv_data, ErlDrvEvent event) {
    rt_state *st = (rt_state *) drv_data;
    (void) event;
    if (st == NULL)
        return;
    (void) driver_output(st->port, "W", 1);
}
#define RT_READY_OUTPUT_SLOT rt_ready_output
#else
#define RT_READY_OUTPUT_SLOT NULL
#endif

static ErlDrvEntry rt_ready_output_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = rt_output,
    .ready_input = NULL,
    .ready_output = RT_READY_OUTPUT_SLOT,
    .driver_name = RT_READY_OUTPUT_NAME,
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
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(RT_READY_OUTPUT_INIT) {
    return &rt_ready_output_entry;
}
"#;

/// The stem mode 0 is compiled and loaded under (its `driver_name` matches).
const STEM_ZERO: &str = "rt_ready_output";
/// The stem mode 1 is compiled and loaded under (its `driver_name` matches).
const STEM_ONE: &str = "rt_ready_output_notify";

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
/// Deliberately a compact **duplicate** of `tests/driver.rs::erl_include_dir`: the integration
/// test's support lives in a separate crate target, which a unit test cannot import, and the
/// resolution is a handful of lines of pure `std` filesystem probing. Resolution order (the first
/// directory that really holds a readable `erl_driver.h` wins): ask the host `erl`; then the
/// `include` sibling of the `erts-*` directory the `erl` on `$PATH` lives in — which is what gets a
/// Nix-style installation, where `code:lib_dir/2` reports a path the store never materialises; then
/// the standard installation roots.
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
    // The `--prefix` release layout keeps the `erts-*` directory under `<root>/lib/erlang`.
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

/// A compiled driver `.so` in a scratch directory.
struct Fixture {
    /// The scratch directory holding the source and the compiled `.so`.
    dir: PathBuf,
    /// The `.so` **without** its suffix (the `DriverPortAdapter::load` candidate; the loader
    /// appends `.so`).
    stem: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// Compiles [`READY_OUTPUT_C`] in `mode` as `<scratch>/<stem>.so`; `None` when a host facility is
/// missing (the caller returns early — **loudly**).
fn build_fixture(mode: u32, stem: &str) -> Option<Fixture> {
    if !cc_available() {
        skip(&format!(
            "ready_output mode {mode}: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip(&format!(
            "ready_output mode {mode}: no erl_driver.h include directory found"
        ));
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("beam_rt_ready_output_{stem}_{}_{n}", std::process::id()));
    let source = dir.join(format!("{stem}.c"));
    let write =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, READY_OUTPUT_C));
    if let Err(error) = write {
        skip(&format!(
            "ready_output mode {mode}: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    // `cc -shared -fPIC -DRT_READY_OUTPUT_MODE=<mode> -I<include> -o <dir>/<stem>.so <dir>/<stem>.c`
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_READY_OUTPUT_MODE={mode}"))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{stem}.so")))
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "ready_output mode {mode}: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "ready_output mode {mode}: could not run the C compiler: {error}"
            ));
            return None;
        }
    }
    if !dir.join(format!("{stem}.so")).is_file() {
        skip(&format!(
            "ready_output mode {mode}: the C compiler produced no {stem}.so"
        ));
        return None;
    }
    Some(Fixture {
        stem: dir.join(stem),
        dir,
    })
}

// ---------------------------------------------------------------------------------------------
// Driving the port
// ---------------------------------------------------------------------------------------------

/// Loads `fixture`'s driver and opens one port on it, returning the poller, registry and id.
///
/// The `black_box` reference is the same anchor `crate::port::PortRegistry::new` uses: it keeps
/// `beam_abi`'s `driver_*` codegen unit in the link, so `dlopen(RTLD_NOW)` can resolve the
/// fixture's undefined `driver_*` imports out of the test binary's dynamic symbol table
/// (`build.rs`'s `-Wl,--export-dynamic`).
fn open_port(fixture: &Fixture, stem: &str) -> (Poller, PortRegistry, PortId) {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let atoms = Rc::new(AtomTable::new());
    let host = DriverPort::new(
        Heap::new(256).expect("a test heap"),
        atoms,
        ProcessId::new(1, 0),
    );
    let adapter = DriverPortAdapter::load(
        &[fixture.stem.display().to_string()],
        stem,
        host,
        String::new(),
    )
    .expect("the fixture driver loads");
    let mut poller = Poller::new().expect("a poller");
    let mut ports = PortRegistry::new();
    let id = ports
        .open(&mut poller, Box::new(adapter))
        .expect("the port opens (the fixture's `start` armed a writable descriptor)");
    (poller, ports, id)
}

/// The bytes a port-task round reported as this port's input (what the owner would receive as
/// `{Port, {data, Data}}`).
fn delivered(notifications: &[PortNotification], id: PortId) -> Vec<u8> {
    let mut data = Vec::new();
    for notification in notifications {
        if let PortNotification::Data { port, bytes } = notification {
            if *port == id {
                data.extend_from_slice(bytes);
            }
        }
    }
    data
}

/// The **discriminating** case: a byte queued by `port_command/2` reaches the driver's `output`
/// even though the driver armed a writable descriptor.
///
/// `start` arms the pipe's write end with `driver_select(…, ERL_DRV_WRITE, 1)`, so when the
/// `OUTPUT` task reaches `ready_output` a descriptor *is* armed for writability. The queued command
/// must still go down the single write path: the driver's `output` echoes it, so the port's input
/// queue carries `ping`.
///
/// Discriminates: a `ready_output` that dispatched only the write-readiness notification (and left
/// the queue for the driver to drain) returned `Ok(0)` and wrote nothing, leaving `drain_input()`
/// empty — this assertion fails on that shape and passes on the C-faithful one.
#[test]
fn a_queued_port_command_reaches_output_with_a_writable_descriptor_armed() {
    let Some(fixture) = build_fixture(0, STEM_ZERO) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture, STEM_ZERO);

    ports
        .port_command(&mut poller, id, b"ping", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    assert_eq!(
        ports.get_mut(id).expect("a live port").output_len(),
        4,
        "port_command queues the bytes and schedules the OUTPUT task"
    );

    let outcome = ports
        .execute_port_tasks(&mut poller, 1024)
        .expect("the port-task round runs");
    assert!(outcome.ran >= 1, "the OUTPUT task ran: {outcome:?}");
    assert_eq!(
        delivered(&outcome.notifications, id),
        b"ping",
        "the queued command reached the driver's output callback (which echoed it back)"
    );
    assert_eq!(
        ports.get_mut(id).expect("a live port").output_len(),
        0,
        "the write path consumed the queue"
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// The C's order: the queued payload goes down the write path **first**, then the entry's
/// `ready_output` slot runs as the write-readiness notification.
///
/// Mode 1's `ready_output` appends a `"W"` marker, so `pingW` pins both halves *and* their order in
/// one observable. The follow-up call drives the adapter directly: `ready_output` returns the
/// number of bytes it wrote (`4`), which the notification-only shape could not — it returned `Ok(0)`
/// and left `abcd` queued.
#[test]
fn ready_output_delivers_the_payload_before_its_notification() {
    let Some(fixture) = build_fixture(1, STEM_ONE) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture, STEM_ONE);

    ports
        .port_command(&mut poller, id, b"ping", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    let outcome = ports
        .execute_port_tasks(&mut poller, 1024)
        .expect("the port-task round runs");
    assert_eq!(
        delivered(&outcome.notifications, id),
        b"pingW",
        "the payload is written first, then the write-readiness notification runs"
    );

    // The same shape one level down: `ready_output` really writes (it reports the byte count).
    // Mode 1's notification does not deselect, so the descriptor is still armed for writability.
    let port = ports.get_mut(id).expect("a live port");
    port.queue_output(b"abcd");
    assert_eq!(
        port.ready_output(&mut poller).expect("the write runs"),
        4,
        "the queued bytes are written even with a writable descriptor armed"
    );
    assert_eq!(
        port.drain_input(),
        b"abcdW",
        "the payload then the notification, both through the port's input queue"
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}
