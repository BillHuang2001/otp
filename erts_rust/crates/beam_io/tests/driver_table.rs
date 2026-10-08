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

//! End-to-end test for the **process-global driver registry and its port-creation service**
//! ([`beam_io::DriverTable`]) over the real, compiled C driver fixture
//! `tests/fixtures/host_driver.c`.
//!
//! This is the proof that the `beam_io` half of the four `driver_*` slots `beam_abi` reports as
//! `None` — `add_driver_entry`/`remove_driver_entry`/`driver_lock_driver`/`driver_create_port` —
//! works against a **real** driver shared object:
//!
//! * the fixture is loaded and its `ErlDrvEntry` registered in a [`DriverTable`]
//!   ([`DriverTable::add_loaded`], the load path's `erts_add_driver_entry`);
//! * a port is created **from the registry** ([`DriverTable::create_port`], the C's
//!   `driver_create_port`), which runs the driver's `start` callback and hands it a stable
//!   `ErlDrvPort` handle;
//! * the driver's `start`/`output`/`stop` callbacks are observed **in order** through the
//!   `log=<path>` command token the fixture appends a line to (`tests/fixtures/host_driver.c`);
//! * `driver_output` from the driver reaches the port owner as port data;
//! * the loaded (dynamic) driver cannot be removed (`-1`, *"Not allowed for dynamic drivers"*)
//!   and **can** be locked (`0`);
//! * a **statically linked** registration and an unknown name are distinguishable, typed
//!   failures (never a fabricated port).
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixture needs host facilities a bare container may lack: a C compiler and the
//! system OTP include directory (`erl_driver.h`). When either is missing the test prints a loud
//! `SKIP:` banner and returns. A load failure is **never** a skip (see `tests/abi_driver.rs`).

mod support;

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use beam_abi::driver::types::ErlDrvEntry;
use beam_io::prelude::*;
use beam_process::PortId;

use support::{cc_available, compile_so, erl_include_dir, scratch_dir, skip};

/// The committed C fixture.
fn fixture_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host_driver.c")
}

/// Compile the fixture into a loadable `.so`, or print a loud `SKIP:` and answer `None` when the
/// toolchain is unavailable.
fn fixture_so() -> Option<PathBuf> {
    let Some(include) = erl_include_dir() else {
        skip("driver_table: no OTP include directory (erl_driver.h) found");
        return None;
    };
    if !cc_available() {
        skip("driver_table: no C compiler (cc / $CC) available");
        return None;
    }
    let src = fixture_source();
    assert!(
        src.is_file(),
        "the committed fixture is missing: {}",
        src.display()
    );
    let dir = scratch_dir("driver-table");
    let so = dir.join("host_driver.so");
    assert!(
        compile_so(&src, &so, &include),
        "the host_driver fixture must compile into a shared object at {}",
        so.display()
    );
    Some(so)
}

/// Pull the `#[no_mangle]` `driver_*` host symbols into this test binary's dynamic symbol table
/// (see `tests/abi_driver.rs` for why the fixture's `dlopen(RTLD_NOW)` needs it).
fn force_driver_symbols_link() {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
}

/// Loads the compiled fixture and registers it in `table`, returning its `ErlDrvEntry`.
///
/// A load failure is a *wiring* failure (the `driver_*` host symbols are not exported), never a
/// missing toolchain — that is [`fixture_so`]'s skip path — so the loader's diagnostic is part of
/// the panic.
fn register_fixture(table: &DriverTable, so: &Path) -> *mut ErlDrvEntry {
    force_driver_symbols_link();
    let path = so.to_str().expect("the fixture path is valid UTF-8");
    let library = match beam_abi::driver::loader::load(path, "host_driver") {
        Ok(library) => library,
        Err(err) => panic!(
            "the host_driver fixture could not be opened with dlopen(RTLD_NOW): {err}\n\
             an `undefined symbol: driver_*` here means the driver_* host symbols are not in this \
             test binary's dynamic symbol table — check build.rs's -Wl,--export-dynamic and the \
             driver_api_table() reference in force_driver_symbols_link()."
        ),
    };
    assert_eq!(library.name(), "host_driver");
    // The entry is the driver's own static data inside the loaded object; the pointer stays valid
    // because the table now owns the `DriverLibrary` (the `dlopen` handle).
    let entry = library.entry() as *const ErlDrvEntry as *mut ErlDrvEntry;
    table
        .add_loaded(library)
        .expect("a loaded driver registers in the table");
    entry
}

/// The lines of the fixture's callback log, or an empty vector when the file does not exist yet
/// (the first `start` append creates it — "no callback has run" is the honest reading).
fn log_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => panic!("the driver's callback log at {} is unreadable: {err}", path.display()),
    }
}

/// The `Data` payloads a port-task round reported for `id`.
fn data_of(outcome: &PortTaskOutcome, id: PortId) -> Vec<Vec<u8>> {
    outcome
        .notifications
        .iter()
        .filter_map(|notification| match notification {
            PortNotification::Data { port, bytes } if *port == id => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

/// Runs one port-task round and returns the bytes of the single [`PortNotification::Data`] it
/// reported for `id`, or `None` when the round reported no data for that port.
fn round_data(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> Option<Vec<u8>> {
    let outcome = ports
        .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the port-task round runs");
    let mut data = data_of(&outcome, id).into_iter();
    let first = data.next();
    assert!(
        data.next().is_none(),
        "one port-task round reports at most one Data notification per port"
    );
    first
}

/// A [`DriverTable::create_port`] from a loaded registration runs the driver's `start` callback,
/// hands it a stable `ErlDrvPort` handle and runs its `stop` callback on close — in that order.
///
/// The order is read back from the fixture's own callback log (`log=<path>`), which the driver
/// appends `start`/`output`/`stop` lines to; the port's data path (an `output` echo that the
/// `upper` token forces to upper case) is the evidence that `start` really parsed the command
/// string it was handed through the created port's `ErlDrvPort`.
#[test]
fn a_registered_driver_creates_a_port_whose_callbacks_run_in_order() {
    let Some(so) = fixture_so() else {
        return;
    };
    let dir = scratch_dir("driver-table-log");
    std::fs::create_dir_all(&dir).expect("a scratch dir is creatable");
    let log = dir.join("events.log");
    let command = format!("upper log={}", log.display());

    let table = DriverTable::new();
    let entry = register_fixture(&table, &so);
    let registered = table.lookup("host_driver").expect("the driver is registered");
    assert!(registered.dynamic, "a loaded driver has a dlopen handle");
    assert!(!registered.locked);

    let mut poller = Poller::new().expect("a poll set");
    let mut ports = PortRegistry::new();
    let id = table
        .create_port(
            &mut ports,
            &mut poller,
            "host_driver",
            &command,
            Box::new(LocalAtomPortHost::new()),
        )
        .expect("the registered driver creates a real port");

    assert!(
        ports.get(id).is_some(),
        "the created port is live in the caller's registry"
    );
    assert_eq!(
        ports.get(id).map(|port| port.driver_name().to_string()),
        Some("host_driver".to_string()),
        "the port reports the registered driver's name"
    );
    assert_eq!(
        log_lines(&log),
        vec!["start".to_string()],
        "creating the port ran the driver's start callback (and nothing else yet)"
    );

    // The driver's `output` callback echoes through `driver_output`, and the `upper` token proves
    // `start` received the command string through the created port's `ErlDrvPort`.
    let written = ports
        .output(&mut poller, id, b"abc")
        .expect("the driver's output callback runs");
    assert_eq!(written, 3, "the driver consumes the whole output buffer");
    assert_eq!(
        round_data(&mut poller, &mut ports, id).expect("the echo reaches the port owner"),
        b"ABC".to_vec(),
        "driver_output reached the port as data"
    );
    assert_eq!(
        log_lines(&log),
        vec!["start".to_string(), "output".to_string()],
        "the driver's output callback ran after start"
    );

    // `driver_lock_driver` on a loaded driver succeeds; `remove_driver_entry` refuses it.
    assert_eq!(table.lock_driver("host_driver"), 0, "a loaded driver locks");
    assert!(
        table.lookup("host_driver").expect("registered").locked,
        "the lock is recorded"
    );
    assert_eq!(
        table.remove_entry(entry),
        -1,
        "a dynamic driver may not be removed"
    );
    assert!(
        table.lookup("host_driver").is_some(),
        "the refused removal left the registration in place"
    );

    ports.close(&mut poller, id).expect("the driver's stop runs");
    assert!(!ports.contains(id), "the closed port is gone");
    assert!(
        poller.is_empty(),
        "the closed port holds no poller registration"
    );
    assert_eq!(
        log_lines(&log),
        vec![
            "start".to_string(),
            "output".to_string(),
            "stop".to_string()
        ],
        "closing the port ran the driver's stop callback last"
    );
}

/// Several ports can be created from **one** registration: the table shares the loaded object
/// (an `Arc<DriverLibrary>`) instead of giving it up to the first port.
#[test]
fn a_second_port_can_be_created_from_the_same_registration() {
    let Some(so) = fixture_so() else {
        return;
    };
    let table = DriverTable::new();
    register_fixture(&table, &so);

    let mut poller = Poller::new().expect("a poll set");
    let mut ports = PortRegistry::new();
    let first = table
        .create_port(
            &mut ports,
            &mut poller,
            "host_driver",
            "",
            Box::new(LocalAtomPortHost::new()),
        )
        .expect("the first port is created");
    let second = table
        .create_port(
            &mut ports,
            &mut poller,
            "host_driver",
            "",
            Box::new(LocalAtomPortHost::new()),
        )
        .expect("a second port is created from the same registration");
    assert_ne!(first, second, "each creation gets its own port instance");

    // Both instances are live and independent.
    for id in [first, second] {
        ports
            .output(&mut poller, id, b"hi")
            .expect("the driver's output callback runs");
        assert_eq!(
            round_data(&mut poller, &mut ports, id).expect("each port echoes its own data"),
            b"hi".to_vec()
        );
        ports.close(&mut poller, id).expect("the port closes");
    }
    assert!(poller.is_empty(), "no registration is left behind");
}

/// The two failure modes of [`DriverTable::create_port`] are distinguishable and never fabricate a
/// port: an **unknown name** is `badarg` (the C's `lookup_driver == NULL`), and a **statically
/// linked** registration is the typed `notsup` gap (Milestone-1 has no `Driver` adapter over a raw
/// `ErlDrvEntry`; the loaded path goes through [`DriverPort::from_library`]).
///
/// This test needs no toolchain: the statically linked entry is a Rust `ErlDrvEntry` with no
/// `init` callback.
#[test]
fn an_unknown_name_and_a_static_entry_are_distinguishable_typed_failures() {
    let table = DriverTable::new();
    let mut poller = Poller::new().expect("a poll set");
    let mut ports = PortRegistry::new();

    let err = table
        .create_port(
            &mut ports,
            &mut poller,
            "no_such_driver",
            "",
            Box::new(LocalAtomPortHost::new()),
        )
        .expect_err("an unknown name is badarg");
    assert_eq!(err.as_atom(), Some("badarg"));

    // A statically linked registration: a bare entry, exactly the C's `add_driver_entry` case.
    let name = CString::new("static_probe").expect("no interior NUL");
    let mut entry = ErlDrvEntry {
        driver_name: name.as_ptr() as *mut libc::c_char,
        ..ErlDrvEntry::default()
    };
    // SAFETY: `entry` is live and named, and `name` outlives every use of the table below.
    unsafe { table.add_entry(&mut entry) }.expect("a named static entry registers");
    assert_eq!(
        table.lookup("static_probe").map(|driver| driver.dynamic),
        Some(false)
    );

    let err = table
        .create_port(
            &mut ports,
            &mut poller,
            "static_probe",
            "",
            Box::new(LocalAtomPortHost::new()),
        )
        .expect_err("a static entry has no driver adapter in Milestone-1");
    assert_eq!(
        err.as_atom(),
        Some("notsup"),
        "the M1 gap is the typed notsup raise, not a wrong port"
    );
    assert!(ports.is_empty(), "no port may exist after a failed creation");
    assert_eq!(table.names(), vec!["static_probe".to_string()]);

    // `remove_driver_entry` removes the static registration (`1`), then reports `0`.
    assert_eq!(table.remove_entry(&mut entry), 1, "removed");
    assert_eq!(table.remove_entry(&mut entry), 0, "not found any more");
    assert!(table.is_empty());
}

/// An interior NUL in the `command` string cannot become the C string `start` receives, so
/// `create_port` refuses it with `badarg` **without** creating a port (the same contract
/// [`DriverPort::load`] has).
#[test]
fn a_command_with_an_interior_nul_is_refused_before_a_port_exists() {
    let Some(so) = fixture_so() else {
        return;
    };
    let table = DriverTable::new();
    register_fixture(&table, &so);

    let mut poller = Poller::new().expect("a poll set");
    let mut ports = PortRegistry::new();
    let err = table
        .create_port(
            &mut ports,
            &mut poller,
            "host_driver",
            "up\0per",
            Box::new(LocalAtomPortHost::new()),
        )
        .expect_err("an interior NUL is badarg");
    assert_eq!(err.as_atom(), Some("badarg"));
    assert!(ports.is_empty());
    assert!(poller.is_empty());
}

/// The registry is not a bottleneck for the runtime's own polling: a created port drives through
/// the ordinary reactor path (`port_command` + a port-task round).
///
/// This is the *runtime-shaped* use of a registry-created port — the same calls `beam_rt` makes —
/// so it pins that the created instance is an ordinary [`Port`] and nothing special.
#[test]
fn a_registry_created_port_drives_through_the_ordinary_reactor_path() {
    let Some(so) = fixture_so() else {
        return;
    };
    let table = DriverTable::new();
    register_fixture(&table, &so);

    let mut poller = Poller::new().expect("a poll set");
    let mut ports = PortRegistry::new();
    let id = table
        .create_port(
            &mut ports,
            &mut poller,
            "host_driver",
            "",
            Box::new(LocalAtomPortHost::new()),
        )
        .expect("the port is created");

    // `port_command` queues the bytes and schedules an OUTPUT task, which dispatches the port's
    // write-readiness seam — the fixture's `ready_output` emits "W".
    ports
        .port_command(&mut poller, id, b"hello", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    let outcome = ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the OUTPUT task runs");
    assert_eq!(outcome.ran, 1, "the queued command schedules one task");
    assert_eq!(
        data_of(&outcome, id),
        vec![b"W".to_vec()],
        "the OUTPUT task ran the driver's ready_output callback"
    );

    // A poll with a bounded timeout is the ordinary drive point and must not error.
    let events = poller
        .poll(Some(Duration::from_millis(1)))
        .expect("the poll succeeds");
    ports.schedule_events(&events);

    ports.close(&mut poller, id).expect("the port closes");
}
