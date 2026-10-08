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

#![allow(dead_code)]
//! Shared helpers for the C-driver integration tests over the `tests/fixtures/host_driver.c`
//! fixture (`tests/abi_driver.rs` and `tests/abi_async.rs`).
//!
//! Like `tests/support/mod.rs`, this module lives in a **subdirectory** of `tests/` so Cargo does
//! not treat it as its own integration-test target: it is compiled into each test binary that pulls
//! it in with `mod cdriver;`.
//!
//! The helpers here are the ones both C-driver suites need: compiling the fixture (and its
//! `ready_async`-less variant) into a loadable `.so`, loading it as a [`DriverPort`] over a test
//! [`PortHost`], opening it as a real port in a [`PortRegistry`] over a real [`Poller`], driving
//! port-task rounds, and reading the fixture's callback log. The `#![allow(dead_code)]` above is
//! the same convention `tests/support/mod.rs` uses (a helper unused by one binary must not warn).

#[path = "../support/mod.rs"]
mod support;

use std::cell::RefCell;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_io::prelude::*;
use beam_io::{DriverPort, PortHost};
use beam_process::PortId;
use beam_term::atom::AtomTable;
use beam_term::immediate::atom_val;
use beam_term::prelude::{make_atom, Eterm};

use support::{cc_available, compile_so, erl_include_dir, scratch_dir, skip};

// ---------------------------------------------------------------------------------------------
// The runtime hook the driver needs: a [`PortHost`] over a private atom table
// ---------------------------------------------------------------------------------------------

/// The state a test observes through [`TestPortHost`].
#[derive(Default)]
pub struct HostState {
    /// The host's atom table.
    ///
    /// `LocalAtomPortHost` interns into a private table too (see its docs): the atoms only have to
    /// be *self-consistent* with the terms built through this same host, which is all this test
    /// needs — a live runtime injects a host over its **shared** atom table instead.
    pub atoms: AtomTable,
    /// Every term `erl_drv_output_term`/`erl_drv_send_term` handed up, with its destination.
    pub sent: Vec<(Option<Eterm>, Eterm)>,
}

/// A [`PortHost`] that interns atoms into its own table and records every term a driver sends up.
pub struct TestPortHost {
    state: Rc<RefCell<HostState>>,
}

impl TestPortHost {
    /// A fresh host plus the shared handle a test reads its recorded terms through.
    pub fn new() -> (TestPortHost, Rc<RefCell<HostState>>) {
        let state = Rc::new(RefCell::new(HostState::default()));
        (
            TestPortHost {
                state: Rc::clone(&state),
            },
            state,
        )
    }
}

impl PortHost for TestPortHost {
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.state.borrow_mut().atoms.intern(name)))
    }

    fn send_term(&mut self, to: Option<Eterm>, term: Eterm) -> bool {
        self.state.borrow_mut().sent.push((to, term));
        true
    }
}

// ---------------------------------------------------------------------------------------------
// Building and loading the C fixture
// ---------------------------------------------------------------------------------------------

/// The directory the committed C fixtures live in.
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The committed C fixture.
pub fn fixture_source() -> PathBuf {
    fixture_dir().join("host_driver.c")
}

/// The committed `ready_async`-less variant fixture (`#include`s `host_driver.c` with the slot
/// forced to `NULL`), the build that exercises the host's `async_free` arm.
pub fn variant_fixture_source() -> PathBuf {
    fixture_dir().join("host_driver_no_ready_async.c")
}

/// Compile the fixture into a loadable `.so`, or print a loud `SKIP:` and answer `None` when the
/// toolchain is unavailable (never a silent pass, never a failure for a missing toolchain).
pub fn fixture_so() -> Option<PathBuf> {
    compile_fixture(&fixture_source(), "host_driver")
}

/// The same for the `ready_async`-less variant fixture.
pub fn variant_fixture_so() -> Option<PathBuf> {
    compile_fixture(&variant_fixture_source(), "host_driver_no_ready_async")
}

/// Compile `src` into `<scratch>/<stem>.so`, skipping loudly when `cc`/`erl_driver.h` are absent.
fn compile_fixture(src: &Path, stem: &str) -> Option<PathBuf> {
    let Some(include) = erl_include_dir() else {
        skip("cdriver: no OTP include directory (erl_driver.h) found");
        return None;
    };
    if !cc_available() {
        skip("cdriver: no C compiler (cc / $CC) available");
        return None;
    }

    assert!(
        src.is_file(),
        "the committed fixture is missing: {}",
        src.display()
    );

    let dir = scratch_dir(stem);
    let so = dir.join(format!("{stem}.so"));
    assert!(
        compile_so(src, &so, &include),
        "the {stem} fixture must compile into a shared object at {}",
        so.display()
    );
    Some(so)
}

/// Force the `#[no_mangle]` `driver_*` host symbols into this test binary's **dynamic** symbol
/// table.
///
/// The fixture leaves `driver_*`/`erl_drv_*` undefined (exactly like a real OTP driver) and
/// resolves them out of the loading process, so its `dlopen(RTLD_NOW)` only succeeds once those
/// symbols are exported. `build.rs` adds `-Wl,--export-dynamic` to the test link, but that only
/// exports symbols that are actually linked in: naming
/// [`beam_abi::driver::drv::driver_api_table`] — the `#[used]` table that holds them — is what
/// pulls `beam_abi`'s `drv` object file into this binary. Without this reference the load fails
/// with `undefined symbol: driver_mk_port`.
pub fn force_driver_symbols_link() {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
}

/// Loads the compiled fixture as a [`DriverPort`] over `host`, failing **loudly** when the object
/// cannot be opened.
///
/// A load failure here is a *wiring* failure — the `driver_*` host symbols are missing from the
/// test binary's dynamic symbol table — not a missing toolchain (that is [`fixture_so`]'s skip
/// path), so the dynamic loader's own diagnostic (typically `undefined symbol: driver_mk_port`) is
/// part of the panic message.
pub fn load_driver(so: &Path, command: &str, host: Box<dyn PortHost>) -> DriverPort {
    force_driver_symbols_link();
    let path = so.to_str().expect("the fixture path is valid UTF-8");
    match beam_abi::driver::loader::load(path, "host_driver") {
        Ok(library) => assert_eq!(
            library.name(),
            "host_driver",
            "a loaded driver reports the name its driver_init entry carries"
        ),
        Err(err) => panic!(
            "the host_driver fixture could not be opened with dlopen(RTLD_NOW): {err}\n\
             an `undefined symbol: driver_*` here means the driver_* host symbols are not in this \
             test binary's dynamic symbol table — check build.rs's -Wl,--export-dynamic and the \
             driver_api_table() reference in force_driver_symbols_link()."
        ),
    }
    DriverPort::load(path, "host_driver", command, host)
        .unwrap_or_else(|err| panic!("the port could not be created from the loaded driver: {err:?}"))
}

/// Opens a port around the fixture loaded with `command`, returning the reactor, the registry and
/// the port id.
pub fn open_port(so: &Path, command: &str, host: TestPortHost) -> (Poller, PortRegistry, PortId) {
    let driver = load_driver(so, command, Box::new(host));
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the loaded driver opens as a port");
    (poller, registry, id)
}

// ---------------------------------------------------------------------------------------------
// Driving the port
// ---------------------------------------------------------------------------------------------

/// The `Data` payloads a port-task round reported for `id`.
pub fn data_of(outcome: &PortTaskOutcome, id: PortId) -> Vec<Vec<u8>> {
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
pub fn round_data(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> Option<Vec<u8>> {
    let outcome = registry
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

/// Polls and runs port-task rounds until the driver reports data for `id`, or `timeout` elapses
/// (a panic — never a silent pass).
///
/// A `poller.poll` with a bounded timeout is what lets both drivers make progress: it delivers a
/// readiness event for a `driver_select`ed descriptor **and** lets wall-clock time pass so a due
/// `TIMEOUT` task can run.
pub fn await_data(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
    timeout: Duration,
) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = poller
            .poll(Some(Duration::from_millis(50)))
            .expect("the poll succeeds");
        registry.schedule_events(&events);
        if let Some(bytes) = round_data(poller, registry, id) {
            return bytes;
        }
        assert!(
            Instant::now() < deadline,
            "the driver reported no data for the port within {timeout:?}"
        );
    }
}

/// The `(arity, elements)` of `term`, which must be a live tuple.
pub fn tuple_parts(term: Eterm) -> (usize, Vec<Eterm>) {
    // SAFETY: `term` was handed up by erl_drv_output_term during a callback on a port that is
    // still alive in this test, so it is a live tuple allocated on that port's driver heap.
    unsafe {
        assert!(beam_term::tuple::is_tuple(term), "the driver built a 2-tuple");
        let arity = beam_term::tuple::tuple_size(term);
        let elements = (0..arity)
            .map(|i| beam_term::tuple::tuple_element(term, i))
            .collect();
        (arity, elements)
    }
}

/// The name of the atom `term` denotes, resolved through the host's own table.
pub fn atom_name(state: &RefCell<HostState>, term: Eterm) -> String {
    let atom = state
        .borrow()
        .atoms
        .name(atom_val(term))
        .map(str::to_string);
    atom.expect("the atom was interned in this host's table")
}

// ---------------------------------------------------------------------------------------------
// The fixture's callback log
// ---------------------------------------------------------------------------------------------

/// The lines the fixture appended to its callback log at `path`, or an empty vector while the
/// file does not exist yet ("no callback has run" is the honest reading).
pub fn log_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => panic!(
            "the driver's callback log at {} is unreadable: {err}",
            path.display()
        ),
    }
}

/// How many lines equal `line` (the "exactly once" assertion every async release test makes).
pub fn count_line(lines: &[String], line: &str) -> usize {
    lines.iter().filter(|entry| entry.as_str() == line).count()
}

/// Polls the fixture log at `path` until some line equals `wanted`, or `timeout` elapses; panics
/// with the log's whole text on timeout (never a silent pass).
///
/// The worker thread that runs a `driver_async` job logs from *its* thread, so a test that must
/// wait for a job's completion polls for the line rather than sleeping a fixed time.
pub fn await_log_line(path: &Path, wanted: &str, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let lines = log_lines(path);
        if lines.iter().any(|entry| entry == wanted) {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "the fixture log at {} never got a `{wanted}` line within {timeout:?}; it holds {lines:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A socket-pair descriptor a driver has been told to `driver_select`, kept **alive** (both ends
/// owned, not just their numbers) for the whole test that owns it.
///
/// The read end of a socket pair stands in for the descriptor a driver would select on, and it is
/// non-blocking for the same reason the `ready_input` tests need it (the fixture's `ready_input`
/// loops `read(2)` down to `EAGAIN` on an edge-triggered reactor). Owning the participant whose fd
/// is selected is the point: returning the bare `RawFd` would drop the `UnixStream` and let another
/// thread's short-lived descriptor recycle that number while the driver calls
/// `driver_select`/`select(2)` on it, which is `EBADF` under the parallel test runner. This value
/// must be held for the duration of the test.
pub struct LoggedPort {
    /// The scratch log path the fixture's `log=<path>` argument points at.
    pub log: PathBuf,
    /// The `fd=<n> log=<path>` command string the port is started with.
    pub command: String,
    /// The participant whose descriptor is selected on — held so its number stays valid.
    pub _read_end: std::os::unix::net::UnixStream,
    /// Its peer, held so the pair stays fully open for the test's lifetime.
    pub _write_end: std::os::unix::net::UnixStream,
}

/// A fresh scratch log path (the directory is created so the fixture's append can succeed) bundled
/// with the command string and the still-open, non-blocking descriptor to select on.
pub fn logged_port_command(stem: &str) -> LoggedPort {
    let dir = scratch_dir(stem);
    std::fs::create_dir_all(&dir).expect("a scratch dir is creatable");
    let log = dir.join("events.log");
    let (read_end, write_end) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
    read_end
        .set_nonblocking(true)
        .expect("the selected descriptor must be non-blocking");
    let fd = read_end.as_raw_fd();
    let command = format!("fd={fd} log={}", log.display());
    LoggedPort {
        log,
        command,
        _read_end: read_end,
        _write_end: write_end,
    }
}