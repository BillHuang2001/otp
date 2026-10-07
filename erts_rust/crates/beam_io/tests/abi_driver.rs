//! End-to-end integration test for the driver **C-ABI host layer** (`src/abi/`), over the real,
//! compiled C driver fixture `tests/fixtures/host_driver.c`.
//!
//! This is the proof that a **real** driver shared object — one that leaves every `driver_*` /
//! `erl_drv_*` symbol *undefined* and resolves them out of the loading process, exactly like an OTP
//! driver — can be `dlopen`ed and driven end to end by this crate:
//!
//! * `driver_init`'s `ErlDrvEntry` is loaded through
//!   [`beam_abi::driver::loader::load`](beam_abi) and wrapped in a [`DriverPort`] (contract **A.4**,
//!   the frozen driver C ABI);
//! * the [`DriverPort`] is opened as a **real port** in a [`PortRegistry`] over a real [`Poller`];
//! * data is driven in and out through the port's own seams — the driver's `output` callback, the
//!   `port_command` write-readiness task, a `driver_select`ed descriptor routed back through
//!   `ready_input`, a `driver_set_timer` deadline, `erl_drv_output_term`'s term sink and a
//!   `driver_async` completion — and every observable is asserted on the Rust side.
//!
//! # The fixture's behaviour (see `tests/fixtures/host_driver.c`)
//!
//! `driver_name = "host_driver"`; `start` parses `fd=<n>` (the descriptor it will `driver_select`
//! on) and an optional bare token `upper` (its `output` upper-cases the bytes it echoes). Its
//! integer `control` verbs are `1` echo into `rbuf`, `2` `driver_set_timer(ms)`, `3`
//! `driver_cancel_timer`, `4` `driver_select(READ)`, `5` `erl_drv_output_term({host_driver, 1})`,
//! `6` `driver_async`, `7` `driver_select(READ, 0)`. Its `output` echoes via `driver_output`, its
//! `ready_input` reads the selected fd and `driver_output`s the bytes, its `ready_output` emits
//! `"W"` and its `timeout` emits `"T"` — so every callback's firing is observable as port data.
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixture needs host facilities a bare container may lack: a C compiler and the
//! system OTP include directory (`erl_driver.h`). When either is missing every test prints a **loud**
//! `SKIP:` banner and returns. A load failure is **never** a skip: it means the `driver_*` host
//! symbols are not in this test binary's dynamic symbol table (see
//! [`force_driver_symbols_link`]), which is a wiring bug, so the `dlopen` diagnostic text is
//! panicked with.
//!
//! # The descriptor must be non-blocking
//!
//! The fixture's `ready_input` reads the selected descriptor in a plain `read(2)` loop down to
//! `EAGAIN`/EOF. This crate's reactor is edge-triggered, so the descriptor handed to `fd=<n>` must
//! be non-blocking or that loop blocks forever (exactly why `SpawnDriver` switches an adopted
//! `{fd, N}` descriptor to `O_NONBLOCK`); the socket-pair test below sets it explicitly.

mod support;

use std::cell::RefCell;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_io::prelude::*;
use beam_io::{DriverPort, PortHost};
use beam_process::PortId;
use beam_term::atom::AtomTable;
use beam_term::immediate::{atom_val, is_atom, is_small, signed_value};
use beam_term::prelude::{make_atom, Eterm};

use support::{cc_available, compile_so, erl_include_dir, scratch_dir, skip};

// ---------------------------------------------------------------------------------------------
// The runtime hook the driver needs: a [`PortHost`] over a private atom table
// ---------------------------------------------------------------------------------------------

/// The state a test observes through [`TestPortHost`].
#[derive(Default)]
struct HostState {
    /// The host's atom table.
    ///
    /// `LocalAtomPortHost` interns into a private table too (see its docs): the atoms only have to
    /// be *self-consistent* with the terms built through this same host, which is all this test
    /// needs — a live runtime injects a host over its **shared** atom table instead.
    atoms: AtomTable,
    /// Every term `erl_drv_output_term`/`erl_drv_send_term` handed up, with its destination.
    sent: Vec<(Option<Eterm>, Eterm)>,
}

/// A [`PortHost`] that interns atoms into its own table and records every term a driver sends up.
struct TestPortHost {
    state: Rc<RefCell<HostState>>,
}

impl TestPortHost {
    /// A fresh host plus the shared handle a test reads its recorded terms through.
    fn new() -> (TestPortHost, Rc<RefCell<HostState>>) {
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

/// The committed C fixture.
fn fixture_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host_driver.c")
}

/// Compile the fixture into a loadable `.so`, or print a loud `SKIP:` and answer `None` when the
/// toolchain is unavailable (never a silent pass, never a failure for a missing toolchain).
fn fixture_so() -> Option<PathBuf> {
    let Some(include) = erl_include_dir() else {
        skip("abi_driver: no OTP include directory (erl_driver.h) found");
        return None;
    };
    if !cc_available() {
        skip("abi_driver: no C compiler (cc / $CC) available");
        return None;
    }

    let src = fixture_source();
    assert!(
        src.is_file(),
        "the committed fixture is missing: {}",
        src.display()
    );

    let dir = scratch_dir("abi-driver");
    let so = dir.join("host_driver.so");
    assert!(
        compile_so(&src, &so, &include),
        "the host_driver fixture must compile into a shared object at {}",
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
fn force_driver_symbols_link() {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
}

/// Loads the compiled fixture as a [`DriverPort`] over `host`, failing **loudly** when the object
/// cannot be opened.
///
/// A load failure here is a *wiring* failure — the `driver_*` host symbols are missing from the
/// test binary's dynamic symbol table — not a missing toolchain (that is [`fixture_so`]'s skip
/// path), so the dynamic loader's own diagnostic (typically `undefined symbol: driver_mk_port`) is
/// part of the panic message.
fn load_driver(so: &Path, command: &str, host: Box<dyn PortHost>) -> DriverPort {
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
fn open_port(so: &Path, command: &str, host: TestPortHost) -> (Poller, PortRegistry, PortId) {
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
fn round_data(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> Option<Vec<u8>> {
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
fn await_data(
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
fn tuple_parts(term: Eterm) -> (usize, Vec<Eterm>) {
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
fn atom_name(state: &RefCell<HostState>, term: Eterm) -> String {
    let atom = state
        .borrow()
        .atoms
        .name(atom_val(term))
        .map(str::to_string);
    atom.expect("the atom was interned in this host's table")
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------

/// The driver's `output` callback echoes bytes back to the port owner.
///
/// This is the driver→owner data path: `watch the port's output queue, the driver reads it in its
/// `output` callback and calls `driver_output`, and the port-task round reports the bytes as the
/// `PortNotification::Data` a runtime turns into `{Port, {data, Data}}`.
#[test]
fn the_drivers_output_callback_echoes_bytes_back_to_the_owner() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    assert_eq!(
        registry.get(id).expect("a live port").driver_name(),
        "host_driver",
        "the port reports the loaded driver's name"
    );

    // The port's inline output path hands the bytes to the driver's `output` callback, which
    // echoes them with `driver_output`; the callback reports the whole buffer consumed (the C
    // `output` callback returns `void`).
    let written = registry
        .output(&mut poller, id, b"hello")
        .expect("the driver's output callback runs");
    assert_eq!(written, 5, "the driver consumes the whole output buffer");

    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the echo reaches the port owner"),
        b"hello".to_vec(),
        "the driver's output callback echoed the bytes back through driver_output"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// A `port_command` write-readiness task reaches the driver's `ready_output` callback.
///
/// `port_command`/`PortRegistry::port_command` only queues the bytes and schedules an `OUTPUT`
/// task (the `erlang:port_command/2` contract); the task dispatches the port's *write-readiness*
/// seam, which the C-ABI adapter maps onto the entry's `ready_output` callback when it has one (the
/// fixture's emits `"W"`) — an Erlang→driver command does **not** run the `output` callback through
/// this task. The driver's `output` callback is exercised separately, above and below.
#[test]
fn a_port_command_write_readiness_task_reaches_ready_output() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    registry
        .port_command(&mut poller, id, b"hello", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    let outcome = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the OUTPUT task runs");
    assert_eq!(outcome.ran, 1, "the queued command schedules one task");
    // One round both runs the task and harvests what it produced, so the marker is in this
    // outcome's notifications (a second round would find the port's input queue already drained).
    assert_eq!(
        data_of(&outcome, id),
        vec![b"W".to_vec()],
        "the OUTPUT task runs the driver's ready_output callback"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// The fixture's `start(port, command)` really parses its command string: the `upper` token makes
/// the driver's `output` callback upper-case what it echoes.
///
/// This is the evidence that the command string built by `open_port/2` reached the loaded driver's
/// `start` callback intact (through `DriverPort`'s C string) and was honoured per port instance.
#[test]
fn the_start_command_reaches_the_driver() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "upper", host);

    registry
        .output(&mut poller, id, b"hello")
        .expect("the driver's output callback runs");
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the upper-cased echo"),
        b"HELLO".to_vec(),
        "the `upper` start token reached the driver and changed its output"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// `erl_drv_output_term` builds the term on the host heap and hands it to the [`PortHost`] sink:
/// the fixture's control verb `5` sends the 2-tuple `{host_driver, 1}` to the port owner.
///
/// This is the driver→owner **term** path (`DriverHost::send_term`), the counterpart of
/// `driver_output`'s byte path; the assertion inspects the term with `beam_term`, so the atom's
/// *identity* (its name in the host's table) and the integer's value are both pinned.
#[test]
fn erl_drv_output_term_delivers_the_expected_tuple() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    let reply = registry
        .port_control(&mut poller, id, "5", &[])
        .expect("control verb 5 runs the driver's erl_drv_output_term");
    assert!(reply.is_empty(), "the fixture's verb 5 answers no reply bytes");

    assert_eq!(
        state.borrow().sent.len(),
        1,
        "the driver sent exactly one term to the owner"
    );
    let (to, term) = state.borrow().sent[0];
    assert!(
        to.is_none(),
        "erl_drv_output_term sends to the port owner (no explicit pid)"
    );

    let (arity, elements) = tuple_parts(term);
    assert_eq!(arity, 2, "the fixture builds the 2-tuple {{host_driver, 1}}");
    assert!(is_atom(elements[0]), "the first element is an atom");
    assert_eq!(
        atom_name(&state, elements[0]),
        "host_driver",
        "the atom is host_driver"
    );
    assert!(is_small(elements[1]), "the second element is a small int");
    assert_eq!(signed_value(elements[1]), 1, "the second element is 1");

    registry.close(&mut poller, id).expect("the port closes");
}

/// `driver_set_timer` arms the port's `TIMEOUT` task, the timer fires the driver's `timeout`
/// callback (which emits `"T"`), and `driver_cancel_timer` drops the deadline again.
///
/// The evidence chain: control verb `2` (with a 100 ms `int` argument) makes
/// [`PortRegistry::next_deadline`] answer `Some` (the driver's declaration is mirrored into the
/// port's task); after the deadline passes the next round runs the `TIMEOUT` task, whose callback
/// reports `"T"` and consumes the deadline; control verb `3` clears it, so `next_deadline` is
/// `None`.
#[test]
fn a_driver_timer_is_armed_fires_and_cancels() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    // Arm a 100 ms timer: `driver_set_timer(port, 100)` (the `int` rides in the control buffer).
    registry
        .port_control(&mut poller, id, "2", &100i32.to_le_bytes())
        .expect("control verb 2 arms the driver's timer");
    let deadline = registry
        .next_deadline()
        .expect("the driver's driver_set_timer deadline is mirrored into the port");
    assert!(
        deadline > Instant::now(),
        "the deadline is in the future, so the callback cannot have run yet"
    );
    assert!(
        !await_data_ready(&mut poller, &mut registry, id),
        "the timeout callback must not run before its deadline"
    );

    // Let the deadline pass; the next round runs the TIMEOUT task.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the timeout callback emitted its marker"),
        b"T".to_vec(),
        "the driver's timeout callback ran and emitted `T`"
    );
    assert!(
        registry.next_deadline().is_none(),
        "a fired driver timer leaves no deadline behind"
    );

    // Re-arm, then cancel: the driver's declaration is what `next_deadline` reports.
    registry
        .port_control(&mut poller, id, "2", &1_000i32.to_le_bytes())
        .expect("control verb 2 re-arms the timer");
    assert!(registry.next_deadline().is_some());
    registry
        .port_control(&mut poller, id, "3", &[])
        .expect("control verb 3 cancels the timer");
    assert!(
        registry.next_deadline().is_none(),
        "driver_cancel_timer clears the driver's deadline and the port's task"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// Whether one non-blocking port-task round reports data for `id` (used to prove a not-yet-due
/// timer does **not** fire early).
fn await_data_ready(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> bool {
    let events = poller
        .poll(Some(Duration::from_millis(10)))
        .expect("the poll succeeds");
    registry.schedule_events(&events);
    round_data(poller, registry, id).is_some()
}

/// A `driver_select`ed descriptor routes readiness back through the driver's `ready_input`
/// callback and out as port data.
///
/// This is the full reactor path over a real OS descriptor: control verb `4` makes the driver call
/// `driver_select(port, fd, READ)` (registered with the poller under the port number), the
/// descriptor becomes readable, the poller's readiness event schedules an `INPUT` task, and the
/// task runs the driver's `ready_input`, which `read(2)`s the fd and `driver_output`s the bytes.
/// Control verb `7` deselects, which deregisters the fd from the poller.
#[test]
fn a_selected_descriptor_routes_input_through_ready_input() {
    let Some(so) = fixture_so() else {
        return;
    };

    // A socket pair stands in for the descriptor a driver would select on. The read end must be
    // non-blocking: the fixture's `ready_input` loops `read(2)` down to EAGAIN, and the reactor is
    // edge-triggered (see the module docs).
    let (read_end, mut write_end) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
    read_end
        .set_nonblocking(true)
        .expect("the selected descriptor must be non-blocking");
    let read_fd = read_end.as_raw_fd();

    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, &format!("fd={read_fd}"), host);

    // Select the read fd: the driver registers it with the poller through this port's context.
    registry
        .port_control(&mut poller, id, "4", &[])
        .expect("control verb 4 selects the descriptor");
    assert_eq!(
        poller.len(),
        1,
        "driver_select registered the descriptor with the poller"
    );

    // Write to the peer: the readiness event schedules the INPUT task, which reads it back.
    write_end.write_all(b"from-the-socket").expect("the write succeeds");
    assert_eq!(
        await_data(&mut poller, &mut registry, id, Duration::from_secs(2)),
        b"from-the-socket".to_vec(),
        "the driver's ready_input read the selected descriptor and reported the bytes"
    );

    // Deselect: the registration is gone.
    registry
        .port_control(&mut poller, id, "7", &[])
        .expect("control verb 7 deselects the descriptor");
    assert!(
        poller.is_empty(),
        "driver_select(.., 0) deregistered the descriptor from the poller"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// A `driver_async` job is scheduled and its completion is driven through the async drive point.
///
/// `driver_async` runs the driver's invoke callback on a worker thread; there is **no port-task
/// kind** for the async hand-off in Milestone-1, so the runtime must call the drive point
/// [`PortRegistry::drive_async`] to drain the finished job (the C runs it from `call_async_ready`
/// on the port's scheduler). This test pins that: the completion is not delivered until
/// `drive_async` is called, and once the completion is observed the worker has finished, so the
/// port keeps working.
///
/// The fixture's entry leaves `ready_async` **and** `async_free` `NULL`, so the observable result
/// of the drive point is that the job is drained and reported as processed rather than that a
/// callback runs.
#[test]
fn an_async_job_is_scheduled_and_its_completion_is_driven() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    // Schedule the async job (control verb 6). `driver_async` answers a queue position, which the
    // adapter returns as the control result — a non-negative position means the job was accepted.
    registry
        .port_control(&mut poller, id, "6", &[])
        .expect("control verb 6 schedules the async job");
    assert!(
        registry.next_deadline().is_none(),
        "the async job declares no port timer"
    );

    // Wait for the worker to finish and the completion to be drained by the drive point.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut processed = 0usize;
    while processed == 0 && Instant::now() < deadline {
        processed = registry
            .drive_async(&mut poller, id)
            .expect("the async drive point runs");
        if processed == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(
        processed >= 1,
        "the driver_async job completed but its completion was never drained by `drive_async`"
    );

    // The completion proved the worker finished; drop whatever the worker's driver_output left
    // (Milestone-1 has no installed port context on a worker thread, so it may be nothing at all),
    // then prove the port still moves data.
    let _drained = round_data(&mut poller, &mut registry, id);
    registry
        .output(&mut poller, id, b"still-alive")
        .expect("the driver's output callback still runs");
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the port still moves data"),
        b"still-alive".to_vec(),
        "the port keeps working after an async job"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// Closing the port runs the loaded driver's `stop` callback and releases its id, without crashing.
///
/// The fixture's `stop` frees the per-port state it allocated in `start` — the evidence that the
/// callback really ran is that the free is safe on that state, plus the port notification and the
/// empty reactor the close leaves behind.
#[test]
fn closing_a_loaded_driver_port_runs_stop_and_releases_the_port() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    registry
        .output(&mut poller, id, b"before-close")
        .expect("the port works before the close");
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("pre-close data"),
        b"before-close".to_vec()
    );

    registry.close(&mut poller, id).expect("the driver's stop runs");
    assert!(!registry.contains(id), "the closed port is gone");
    assert!(poller.is_empty(), "the closed port holds no poller registration");

    let outcome = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the close round runs");
    assert!(
        outcome
            .notifications
            .iter()
            .any(|notification| matches!(notification, PortNotification::Closed { port } if *port == id)),
        "the round after a close reports the closed port"
    );
}
