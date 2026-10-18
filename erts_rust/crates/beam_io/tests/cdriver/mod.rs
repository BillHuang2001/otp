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

/// The committed `outputv`-only fixture (`vector_driver.c`): `.output = NULL`,
/// `.outputv = vector_outputv`, `.ready_output = NULL` and `.ready_async = NULL`.
pub fn vector_fixture_source() -> PathBuf {
    fixture_dir().join("vector_driver.c")
}

/// The committed `outputv`+`output`+`ready_output` fixture (`host_driver_both.c`): `host_driver.c`
/// with the entry's `outputv` slot set to `host_outputv`, so all three write-side callbacks are
/// declared at once (the C's `outputv`-over-`output` preference, `io.c:1782`).
pub fn both_fixture_source() -> PathBuf {
    fixture_dir().join("host_driver_both.c")
}

/// The committed **shape** fixture (`output_shape_driver.c`): a self-contained driver whose
/// `output` callback makes the `driver_output*`/`driver_failure*` call that the **first command
/// byte** names (its opcode table is in the fixture's own file header). It is the object
/// `tests/abi_failure.rs` drives the output-shape and failure-channel rows with.
pub fn shape_fixture_source() -> PathBuf {
    fixture_dir().join("output_shape_driver.c")
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

/// The committed `process_exit`-less variant fixture (`#include`s `host_driver.c` with the entry's
/// `process_exit` slot forced to `NULL`), the build that exercises the host's port-exit adapter's
/// `emergency_close` fallback.
pub fn no_process_exit_variant_fixture_source() -> PathBuf {
    fixture_dir().join("host_driver_no_process_exit.c")
}

/// The same for the `process_exit`-less variant fixture.
pub fn no_process_exit_variant_fixture_so() -> Option<PathBuf> {
    compile_fixture(
        &no_process_exit_variant_fixture_source(),
        "host_driver_no_process_exit",
    )
}

/// The same for the `outputv`-only fixture.
pub fn vector_fixture_so() -> Option<PathBuf> {
    compile_fixture(&vector_fixture_source(), "vector_driver")
}

/// The same for the both-write-callbacks fixture (`host_driver_both.c`).
pub fn both_fixture_so() -> Option<PathBuf> {
    compile_fixture(&both_fixture_source(), "host_driver_both")
}

/// The same for the shape fixture (`output_shape_driver.c`), whose entry's `driver_name` is
/// `output_shape_driver`.
pub fn shape_fixture_so() -> Option<PathBuf> {
    compile_fixture(&shape_fixture_source(), "output_shape_driver")
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
    load_named_driver(so, "host_driver", command, host)
}

/// The same, for a driver whose `ErlDrvEntry.driver_name` is `name` (the loader validates it).
pub fn load_named_driver(
    so: &Path,
    name: &str,
    command: &str,
    host: Box<dyn PortHost>,
) -> DriverPort {
    force_driver_symbols_link();
    let path = so.to_str().expect("the fixture path is valid UTF-8");
    match beam_abi::driver::loader::load(path, name) {
        Ok(library) => assert_eq!(
            library.name(),
            name,
            "a loaded driver reports the name its driver_init entry carries"
        ),
        Err(err) => panic!(
            "the {name} fixture could not be opened with dlopen(RTLD_NOW): {err}\n\
             an `undefined symbol: driver_*` here means the driver_* host symbols are not in this \
             test binary's dynamic symbol table — check build.rs's -Wl,--export-dynamic and the \
             driver_api_table() reference in force_driver_symbols_link()."
        ),
    }
    DriverPort::load(path, name, command, host)
        .unwrap_or_else(|err| panic!("the port could not be created from the loaded driver: {err:?}"))
}

/// Opens a port around the fixture loaded with `command`, returning the reactor, the registry and
/// the port id.
pub fn open_port(so: &Path, command: &str, host: TestPortHost) -> (Poller, PortRegistry, PortId) {
    open_named_port(so, "host_driver", command, host)
}

/// The same, for a driver whose entry `driver_name` is `name`.
pub fn open_named_port(
    so: &Path,
    name: &str,
    command: &str,
    host: TestPortHost,
) -> (Poller, PortRegistry, PortId) {
    let driver = load_named_driver(so, name, command, Box::new(host));
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

/// The `Data` payloads a port-task round reported for `id`, flattened exactly as a `[list]` port's
/// `{Port, {data, Data}}` would carry them (`ReadMessage::flattened`).
///
/// One element per driver `driver_output*` call, in arrival order: the C delivers one
/// `{Port, {data, _}}` message per call (`deliver_read_message`, `io.c:3363-3442`), so two calls
/// inside one callback are **two** elements — they are never merged into one (which is the
/// coalescing bug the assertions this helper is used by were corrected for).
pub fn data_of(outcome: &PortTaskOutcome, id: PortId) -> Vec<Vec<u8>> {
    outcome
        .notifications
        .iter()
        .filter_map(|notification| match notification {
            PortNotification::Data { port, read } if *port == id => Some(read.flattened()),
            _ => None,
        })
        .collect()
}

/// The **full** [`ReadMessage`]s a port-task round reported for `id`, in arrival order.
///
/// This is the split-preserving companion of [`data_of`]: the header stays apart from the body and
/// a `None` body stays distinguishable from an empty one, which is what the `driver_output2`
/// shape rows are about.
pub fn reads_of(outcome: &PortTaskOutcome, id: PortId) -> Vec<ReadMessage> {
    outcome
        .notifications
        .iter()
        .filter_map(|notification| match notification {
            PortNotification::Data { port, read } if *port == id => Some(read.clone()),
            _ => None,
        })
        .collect()
}

/// How many `PortNotification::Closed` reports a round minted for `id`.
///
/// A close is reported **exactly once** — the report is also what retires the port's number — so a
/// count above one would mean a second, independent death report for a port that can only die once.
pub fn closed_of(outcome: &PortTaskOutcome, id: PortId) -> usize {
    outcome
        .notifications
        .iter()
        .filter(|notification| matches!(notification, PortNotification::Closed { port } if *port == id))
        .count()
}

/// Runs one port-task round and returns the bytes of the **only** [`PortNotification::Data`] it
/// reported for `id`, or `None` when the round reported no data for that port.
///
/// The `only` is the **call site's** expectation, not a rule of the round: a round reports one
/// `Data` per driver `driver_output*` call, so a callback making two calls reports two messages
/// and a round may carry several for one port. Every caller of this helper is a single-call case;
/// a multi-call one must use [`data_of`]/[`reads_of`] and assert the whole vector instead.
pub fn round_data(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> Option<Vec<u8>> {
    let outcome = registry
        .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the port-task round runs");
    let mut data = data_of(&outcome, id).into_iter();
    let first = data.next();
    assert!(
        data.next().is_none(),
        "this round must report exactly one Data notification for the port: the driver made \
         exactly one driver_output* call (a second one would mean two calls were coalesced, or \
         this call site now makes more than one)"
    );
    first
}

/// The data frames the fixture's **`driver_async` job path** may hand up on its own, i.e. besides
/// whatever the test's own call produced.
///
/// * `b"A"` — the worker-thread invoke callback (`host_async_invoke`, `tests/fixtures/host_driver.c`)
///   `driver_output`s it;
/// * `b"H"` — the completion callback (`host_ready_async`) `driver_output`s it when the entry has a
///   `ready_async` (the default fixture does; the `host_driver_no_ready_async.c` variant does not).
///
/// Both are asynchronous deliveries relative to the caller's own callback — see
/// [`drain_data`]/[`await_trailing_data`] — so a test must never assume a fixed number of them.
pub const ASYNC_JOB_FRAMES: [&[u8]; 2] = [b"A", b"H"];

/// Whether `frame` is one of the fixture's own `driver_async`-path frames ([`ASYNC_JOB_FRAMES`]).
pub fn is_async_job_frame(frame: &[u8]) -> bool {
    ASYNC_JOB_FRAMES.iter().any(|payload| *payload == frame)
}

/// How many consecutive rounds [`drain_data`] may run before it declares the port stuck (a driver
/// that keeps handing up data would otherwise spin forever).
const MAX_DRAIN_ROUNDS: usize = 16;

/// Runs port-task rounds until a round reports **no** data for `id`, returning every frame the
/// rounds reported for it, in arrival order.
///
/// This is the right shape whenever the port owes a **variable** number of frames — a
/// `driver_async` job's frames (see [`ASYNC_JOB_FRAMES`]) land on the port's pending-read queue
/// whenever the worker thread's `driver_output*` happens to coincide with one of the port thread's
/// driver callbacks, so "how many are owed right now" is not something a call site knows.
///
/// Each frame is still **one** `driver_output*` call — the no-coalescing rule [`round_data`]
/// asserts — only the *count* is open here. A call site that owns its own single driver call keeps
/// using [`round_data`]; a call site that must prove something about the sequence uses this (or
/// [`await_trailing_data`]) and asserts the whole vector.
pub fn drain_data(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    for _ in 0..MAX_DRAIN_ROUNDS {
        let outcome = registry
            .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the port-task round runs");
        let round = data_of(&outcome, id);
        if round.is_empty() {
            return frames;
        }
        frames.extend(round);
    }
    panic!("the port still reported data after {MAX_DRAIN_ROUNDS} drain rounds: {frames:?}");
}

/// Runs port-task rounds until the port reports `expected` as its **newest** frame, returning every
/// frame the rounds reported for it, in arrival order (so the caller can assert that `expected` is
/// the last one and inspect what led up to it).
///
/// The tail semantics are the point: a `driver_async` job's frames ([`ASYNC_JOB_FRAMES`]) are
/// delivered asynchronously with respect to the caller's own driver call, so the frame `expected`
/// closes a sequence whose *prefix* is whatever the job still owed — never the other way round, because
/// the C delivers in call order (one `{Port, {data, _}}` per `driver_output*`, `io.c:6387-6446`).
///
/// Panics — never a silent pass — when `expected` does not arrive within `timeout`.
pub fn await_trailing_data(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
    expected: &[u8],
    timeout: Duration,
) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut frames: Vec<Vec<u8>> = Vec::new();
    loop {
        let events = poller
            .poll(Some(Duration::from_millis(5)))
            .expect("the poll succeeds");
        registry.schedule_events(&events);
        let outcome = registry
            .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the port-task round runs");
        frames.extend(data_of(&outcome, id));
        if frames
            .last()
            .is_some_and(|frame| frame.as_slice() == expected)
        {
            return frames;
        }
        assert!(
            Instant::now() < deadline,
            "the port never reported {expected:?} as its newest frame within {timeout:?}; \
             it reported {frames:?}"
        );
    }
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

/// One fixture port's whole test-side setup: the scratch log path its `log=<path>` argument points
/// at, the `fd=<n> log=<path>` command string, and the socket pair whose read end is the descriptor
/// the fixture `driver_select`s.
///
/// The read end of a socket pair stands in for the descriptor a driver would select on, and it is
/// non-blocking for the same reason the `ready_input` tests need it (the fixture's `ready_input`
/// loops `read(2)` down to `EAGAIN` on an edge-triggered reactor). Owning the participant whose fd
/// is selected is the point: returning the bare `RawFd` would drop the `UnixStream` and let another
/// thread's short-lived descriptor recycle that number while the driver calls
/// `driver_select`/`select(2)` on it, which is `EBADF` under the parallel test runner. This value
/// must be held for the duration of the test.
///
/// It also **owns the log path's lifetime**: the scratch directory is created here and removed by
/// [`Drop`], so the only file a round can write is one the test itself cleans up afterwards. Every
/// log read (`log_lines`/`await_log_line`/`settle_log`) must therefore happen while the value is
/// alive, which is what every test does (the log is read before the port value goes out of scope).
pub struct LoggedPort {
    /// The scratch directory holding [`LoggedPort::log`], removed when this value drops.
    dir: PathBuf,
    /// The scratch log path the fixture's `log=<path>` argument points at (absolute: the fixture
    /// honours only absolute paths, so nothing it writes can land in the process's cwd).
    pub log: PathBuf,
    /// The `fd=<n> log=<path>` command string the port is started with.
    pub command: String,
    /// The participant whose descriptor is selected on — held so its number stays valid.
    pub _read_end: std::os::unix::net::UnixStream,
    /// Its peer, held so the pair stays fully open for the test's lifetime.
    pub _write_end: std::os::unix::net::UnixStream,
}

impl Drop for LoggedPort {
    fn drop(&mut self) {
        // Best effort: a teardown problem leaves a scratch directory behind, it must never turn a
        // finished assertion into a panic (a panic in `drop` during an unwinding test aborts).
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A fresh scratch log path (the directory is created so the fixture's append can succeed) bundled
/// with the command string and the still-open, non-blocking descriptor to select on.
pub fn logged_port_command(stem: &str) -> LoggedPort {
    let dir = scratch_dir(stem);
    std::fs::create_dir_all(&dir).expect("a scratch dir is creatable");
    let log = dir.join("events.log");
    assert!(
        log.is_absolute(),
        "the fixture only honours an absolute `log=` path, got {}",
        log.display()
    );
    let (read_end, write_end) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
    read_end
        .set_nonblocking(true)
        .expect("the selected descriptor must be non-blocking");
    let fd = read_end.as_raw_fd();
    let command = format!("fd={fd} log={}", log.display());
    LoggedPort {
        dir,
        log,
        command,
        _read_end: read_end,
        _write_end: write_end,
    }
}

// ---------------------------------------------------------------------------------------------
// Driving the async completion arms (shared by `tests/abi_async.rs` and `tests/abi_vector.rs`)
// ---------------------------------------------------------------------------------------------

/// How long a test waits for a worker-thread log line or a completion to be drained.
///
/// Generous relative to the work (the slow job sleeps ~500 ms): it only has to absorb a loaded test
/// runner, so a real hang still fails rather than passing slowly.
pub const ASYNC_TIMEOUT: Duration = Duration::from_secs(10);

/// Drives the port's async completions until at least one job is processed, or `timeout` elapses
/// (a panic — never a silent pass).
///
/// The drive point is [`PortRegistry::drive_async`](beam_io::PortRegistry::drive_async), which
/// `DriverPort` maps onto the C entry's `ready_async`/`async_free` (there is no `PortTaskKind::Async`
/// in M1).
pub fn drive_until_drained(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
    timeout: Duration,
) -> usize {
    drive_until(poller, registry, id, 1, timeout)
}

/// Drives the port's async completions until at least `wanted` jobs have been processed, or
/// `timeout` elapses (a panic — never a silent pass).
///
/// A completion is a single value the drive point hands to exactly one arm, so the count it returns
/// is a one-per-job count: waiting for `wanted` of them is how a test proves *each* scheduled job
/// was delivered.
pub fn drive_until(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
    wanted: usize,
    timeout: Duration,
) -> usize {
    let deadline = Instant::now() + timeout;
    let mut processed = 0usize;
    loop {
        processed += registry
            .drive_async(poller, id)
            .expect("the async drive point runs");
        if processed >= wanted {
            return processed;
        }
        assert!(
            Instant::now() < deadline,
            "only {processed} of {wanted} driver_async completions were drained within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits a short, bounded time and re-reads the log, so a "runs exactly once" assertion cannot be
/// fooled by a second release that lands a moment after the first.
pub fn settle_log(path: &Path) -> Vec<String> {
    std::thread::sleep(Duration::from_millis(150));
    log_lines(path)
}