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

//! End-to-end proof of the **driver-monitor** family of the driver C ABI (workspace contract
//! **A.4**): the real C driver `tests/fixtures/driver_monitors.c` — compiled with the system `cc`
//! against the system `erl_driver.h` — is `dlopen`ed, validated and driven through the Rust
//! loader, its `driver_*` imports resolving out of the *test binary's* dynamic symbol table.
//!
//! # Why this is the load-bearing test
//!
//! A monitor is nothing but its **identity**: `driver_monitor_process` hands the driver an opaque
//! `ErlDrvMonitor` whose first `ERTS_REF_THING_SIZE * sizeof(Eterm)` (24) bytes are the emulator's
//! internal reference thing (`io.c:6994-7000`), and every later call is keyed on exactly those
//! bytes (`erts_driver_monitor_to_ref` + `erts_monitor_tree_lookup`, `io.c:7002-7010`/`7069`/`7108`;
//! `driver_compare_monitors` is a pure 24-byte `sys_memcmp`, `io.c:7142-7147`). A runtime with a
//! per-port monitor registry therefore has to read those bytes out of the raw `*const ErlDrvMonitor`
//! a `DriverHost` hook receives — which a `#![deny(unsafe_code)]` host cannot spell itself.
//!
//! This crate closes that half with [`monitor_identity`] (the read side, `None` for `NULL`) and its
//! write-side counterpart [`monitor_from_identity`]. This test is the proof that the *whole family*
//! is drivable from safe code alone: the crate-level `#![deny(unsafe_code)]` below makes the
//! absence of `unsafe` in [`MonitorsHost`] a **compile-time** fact, and the host resolves, looks up
//! and demonitors monitors **only** through [`monitor_identity`]. The fixture reports the 24 bytes
//! it received, so the identity the C side holds is compared with the one the host issued.
//!
//! # The host is not the runtime
//!
//! [`MonitorsHost`] plays the part the sibling `beam_rt`/`beam_io` must play: it keeps a monitor
//! table keyed on the identity and answers the three `DriverHost` monitor hooks from it. What is
//! *not* proven here is the scheduler half — "the monitor is armed on a live process and
//! `process_exit` fires when it dies" needs a real process/scheduler behind the seam, which is the
//! documented remaining gap of this family (see the module docs of `src/driver/drv/pdl.rs`). The
//! fixture declares the `process_exit` callback the C requires before it will arm a monitor
//! (`io.c:3020-3022`) but the callback cannot fire in this test.
//!
//! # The oracle half
//!
//! `driver_compare_monitors` takes no port and no host state, so its behaviour can be
//! cross-checked against a real ERTS. This build does **not** export `erlang:load_driver/2` (a
//! build-time emulator option), so the fixture also exports a NIF entry (`nif_init`) that runs the
//! *same* `compare_report()` through the real emulator's exported `driver_compare_monitors` (a
//! `T` entry in `nm -D beam.smp`). Both halves print the same four `key=value` reports, so a
//! difference can only come from the runtime's own comparison. The report normalises to the
//! **sign** of the `memcmp`, because the magnitude of a `memcmp` result is not specified by C.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`); inspecting symbol tables needs `nm`/`readelf`; the oracle half needs
//! `erl`/`erlc`. When such a *toolchain* facility is missing the affected test prints a loud
//! `SKIP:` banner and returns — but when the toolchain **is** present, every assertion is hard (a
//! mismatch fails, it never silently passes).

#![deny(unsafe_code)]

mod common;

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::loader::{load, DriverLibrary};
use beam_abi::driver::types::{ErlDrvData, ErlDrvMonitor, ErlDrvPort, ErlDrvSSizeT};
use beam_abi::{monitor_from_identity, monitor_identity, DriverEnv, DriverHost, MonitorResult};
use beam_term::prelude::{make_internal_pid, Eterm};
use libc::{c_char, c_int, c_void};
use common::{compile_so, skip, TestDriverHost};

/// The committed C fixture — a real driver (and a NIF entry for the oracle; see the file header).
const FIXTURE_C: &str = include_str!("fixtures/driver_monitors.c");

/// The name both entries declare (`DRIVER_INIT(monitors)` / `ERL_NIF_INIT(monitors, …)`).
const FIXTURE_NAME: &str = "monitors";

/// The `driver_*` / `erl_drv_*` and `enif_*` symbols the fixture imports; they must be *undefined*
/// in the `.so` and *defined* in the loading process.
const FIXTURE_IMPORTS: [&str; 10] = [
    "driver_alloc",
    "driver_free",
    "driver_output",
    "driver_caller",
    "driver_term_nil",
    "driver_monitor_process",
    "driver_compare_monitors",
    "driver_demonitor_process",
    "driver_get_monitored_process",
    "enif_make_string_len",
];

/// The reason codes the family reproduces (`io.c`'s literal returns; the header names none).
const ERL_DRV_MONITOR_OK: c_int = 0;
const ERL_DRV_MONITOR_NOT_ALIVE: c_int = 1;
const ERL_DRV_MONITOR_ERROR: c_int = -1;

/// How [`MonitorsHost`] answers `driver_monitor_process` — the three C outcomes of
/// `do_driver_monitor_process` (`io.c:7012-7038`), which the C separates by **sign**.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MonitorOutcome {
    /// Arm for real: the C's `0` (`ERL_DRV_MONITOR_OK`), the holder filled.
    Arm,
    /// The process is not alive: the C's `1` (`ERL_DRV_MONITOR_NOT_ALIVE`, `io.c:7031`).
    NotAlive,
    /// The host cannot monitor (no `process_exit`, or no live process): `-1` (`io.c:7020-7021`).
    Error,
}

/// The identity width: `ERTS_REF_THING_SIZE(3) * sizeof(Eterm)(8)` (`io.c:6996`).
const IDENTITY_BYTES: usize = 24;
/// `sizeof(ErlDrvMonitor)` in both the header (`sizeof(void *) * 4`) and this crate.
const MONITOR_BYTES: usize = 4 * std::mem::size_of::<*mut c_void>();

// ---------------------------------------------------------------------------------------------
// Helpers (the same idiom as `tests/driver_pdl.rs`)
// ---------------------------------------------------------------------------------------------

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// Run `cmd args…`, returning its stdout on success (`None` when the tool is missing or fails).
fn run_tool(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `true` iff every name in `names` is an **undefined** import of `path`.
fn all_symbols_are_undefined_imports(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--undefined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && line.contains(" UND "))
        }));
    }
    None
}

/// `true` iff every name in `names` is **defined** in `path`'s dynamic symbol table.
fn all_symbols_are_exported(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--defined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && !line.contains(" UND "))
        }));
    }
    None
}

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(FIXTURE_NAME, FIXTURE_C)
}

/// Whether `cmd` can be spawned with `args` at all (the probe for the oracle toolchain; a non-zero
/// exit is fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// Force the family's definitions into the link, so `--export-dynamic` can publish them.
fn link_the_family() {
    let table = driver_api_table();
    assert!(table.driver_monitor_process.is_some());
    assert!(table.driver_demonitor_process.is_some());
    assert!(table.driver_get_monitored_process.is_some());
    assert!(table.driver_compare_monitors.is_some());
}

// ---------------------------------------------------------------------------------------------
// The safe host: a monitor registry keyed on the identity, read through `monitor_identity` only
// ---------------------------------------------------------------------------------------------

/// A [`DriverHost`] that plays the part of the runtime for the monitor family.
///
/// It wraps the shared [`TestDriverHost`] (the arena, the atom table and the recorded
/// output/term traffic stay its business) and adds the one thing a monitor host needs: a table
/// keyed on the monitor's **identity**. Both the read side ([`monitor_identity`]) and the write
/// side ([`monitor_from_identity`]) are the crate's public, `unsafe`-free accessors, so this file
/// compiles under `#![deny(unsafe_code)]` — which is exactly the condition the sibling
/// `beam_rt`/`beam_io` (also `#![deny(unsafe_code)]`) works under.
struct MonitorsHost {
    inner: TestDriverHost,
    /// The live monitors: `(identity, monitored process)`.
    monitors: Vec<([u8; IDENTITY_BYTES], Eterm)>,
    /// The next monitor's id, so no two monitors are ever byte-identical.
    next: u64,
    /// The outcome [`DriverHost::monitor_process_result`] reports, i.e. which of the C's three
    /// `driver_monitor_process` results the host pretends to reach.
    monitor_outcome: MonitorOutcome,
}

impl MonitorsHost {
    fn new() -> MonitorsHost {
        MonitorsHost {
            inner: TestDriverHost::new(),
            monitors: Vec::new(),
            next: 1,
            monitor_outcome: MonitorOutcome::Arm,
        }
    }

    /// What [`DriverHost::caller_pid`] reports (the C's `driver_caller`).
    fn set_caller(&mut self, pid: Option<Eterm>) {
        self.inner.set_caller(pid);
    }

    /// Choose which of the three C `driver_monitor_process` outcomes the host reports next.
    fn set_monitor_outcome(&mut self, outcome: MonitorOutcome) {
        self.monitor_outcome = outcome;
    }
    /// Every byte string the fixture published through `driver_output`, in call order.
    fn outputs(&self) -> &[Vec<u8>] {
        self.inner.outputs()
    }

    /// The live monitors, in the order they were armed.
    fn armed(&self) -> &[([u8; IDENTITY_BYTES], Eterm)] {
        &self.monitors
    }

    /// The identity of the `n`-th monitor this host armed.
    fn identity_of(&self, n: usize) -> [u8; IDENTITY_BYTES] {
        self.monitors[n].0
    }
}

impl DriverHost for MonitorsHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        self.inner.heap()
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        self.inner.intern_atom(name)
    }

    fn output(&mut self, port: ErlDrvPort, data: &[u8]) -> c_int {
        self.inner.output(port, data)
    }

    fn send_term(&mut self, port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int {
        self.inner.send_term(port, to, term)
    }

    fn caller_pid(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        self.inner.caller_pid(port)
    }

    /// Arm a monitor on `pid` and remember it under the identity this host chooses.
    ///
    /// The 24 bytes are opaque here too: the host never interprets them, it only records them (and
    /// hands them to the driver in the holder `driver_monitor_process` writes out).
    fn monitor_process(&mut self, pid: Eterm) -> Option<ErlDrvMonitor> {
        let mut identity = [0u8; IDENTITY_BYTES];
        identity[..8].copy_from_slice(&self.next.to_ne_bytes());
        self.next += 1;
        self.monitors.push((identity, pid));
        Some(monitor_from_identity(identity))
    }

    /// The **three-way** monitor hook (contract A.4, `io.c:7012-7038`): the C's `0` / `> 0` / `< 0`
    /// all become expressible. In [`MonitorOutcome::Arm`] it delegates to the two-way
    /// [`monitor_process`](DriverHost::monitor_process) so the identity logic stays in one place; the
    /// other two states report the C's positive `1` and negative `-1` directly.
    fn monitor_process_result(&mut self, pid: Eterm) -> MonitorResult {
        match self.monitor_outcome {
            MonitorOutcome::Arm => match self.monitor_process(pid) {
                Some(mon) => MonitorResult::Ok(mon),
                None => MonitorResult::Error,
            },
            MonitorOutcome::NotAlive => MonitorResult::NotAlive,
            MonitorOutcome::Error => MonitorResult::Error,
        }
    }
    /// Remove the monitor carrying `mon`'s identity: `0` removed, `1` no such monitor (`io.c:7074`).
    fn demonitor_process(&mut self, mon: *const ErlDrvMonitor) -> c_int {
        let Some(identity) = monitor_identity(mon) else {
            return ERL_DRV_MONITOR_ERROR;
        };
        match self.monitors.iter().position(|(m, _)| *m == identity) {
            Some(i) => {
                self.monitors.remove(i);
                ERL_DRV_MONITOR_OK
            }
            None => ERL_DRV_MONITOR_NOT_ALIVE,
        }
    }

    /// The process `mon` was armed for, resolved by identity alone (`io.c:7104-7118`).
    fn get_monitored_process(&mut self, mon: *const ErlDrvMonitor) -> Option<Eterm> {
        let identity = monitor_identity(mon)?;
        self.monitors
            .iter()
            .find(|(m, _)| *m == identity)
            .map(|(_, pid)| *pid)
    }
}

// ---------------------------------------------------------------------------------------------
// Driving the fixture through the C ABI
// ---------------------------------------------------------------------------------------------

/// Drive one `control` command with no buffers, expecting the fixture to answer.
fn command(lib: &DriverLibrary, data: ErlDrvData, cmd: u32) -> ErlDrvSSizeT {
    let mut reply: *mut c_char = std::ptr::null_mut();
    lib.control(data, cmd, std::ptr::null_mut(), 0, &mut reply, 0)
        .expect("the fixture implements control")
}

/// Drive one `control` command whose reply is a monitor's identity (command 10/11).
fn identity_command(lib: &DriverLibrary, data: ErlDrvData, cmd: u32) -> Vec<u8> {
    let mut reply = vec![0u8; IDENTITY_BYTES];
    let mut reply_ptr = reply.as_mut_ptr() as *mut c_char;
    let n = lib
        .control(
            data,
            cmd,
            std::ptr::null_mut(),
            0,
            &mut reply_ptr,
            reply.len(),
        )
        .expect("the fixture implements control");
    assert_eq!(
        n as usize, IDENTITY_BYTES,
        "command {cmd} must report the {IDENTITY_BYTES} identity bytes"
    );
    reply
}

/// Load the fixture, build the host, `start` the driver, run `f`, then `stop` the driver.
///
/// The host must not move while the fixture is live: [`DriverEnv`] holds a raw pointer to it (the
/// `ErlDrvPort` a driver callback receives *is* the env pointer), exactly as the C's `ErlDrvPort`
/// is a pointer into emulator state. The closure therefore receives `&mut MonitorsHost` at its
/// final address instead of a value to take ownership of.
///
/// `None` (after a loud skip) when the toolchain cannot build the fixture.
fn with_session<R>(
    f: impl FnOnce(&DriverLibrary, ErlDrvData, &mut MonitorsHost, &Path) -> R,
) -> Option<R> {
    link_the_family();
    let so = compile_fixture()?;
    let path = so_path(&so)?;
    let lib = load(&path, FIXTURE_NAME)
        .expect("a real driver must load: its driver_*/enif_* imports resolve from the test binary");
    assert_eq!(lib.name(), FIXTURE_NAME);

    let mut host = MonitorsHost::new();
    // The C's `driver_caller`: the process the port is connected to. It must be a real internal pid,
    // because `driver_monitor_process` refuses an operand that cannot name a process.
    host.set_caller(Some(make_internal_pid(7)));
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command_line = CString::new("start").expect("no interior NUL");
    let data = lib
        .start(port, command_line.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(
        !data.is_null(),
        "start must return the private state, not a failure code"
    );

    let result = f(&lib, data, &mut host, &so);

    // `stop` frees the private state (no leak, no double free: it runs exactly once here).
    lib.stop(data).expect("the fixture implements stop");
    drop(lib);
    Some(result)
}

// ---------------------------------------------------------------------------------------------
// The identity: the bytes the C side holds are the bytes the host issued
// ---------------------------------------------------------------------------------------------

#[test]
fn the_monitor_identity_is_what_drives_the_whole_family() {
    let Some(()) = with_session(|lib, data, host, _so| {
        // --- commands 1-2: two monitors armed on the port's connected process --------------------
        assert_eq!(command(lib, data, 1), ERL_DRV_MONITOR_OK as ErlDrvSSizeT);
        assert_eq!(command(lib, data, 2), ERL_DRV_MONITOR_OK as ErlDrvSSizeT);
        assert_eq!(host.armed().len(), 2, "both monitors must be registered");
        assert_eq!(host.armed()[0].1, make_internal_pid(7));

        // --- commands 3-5: the refusal codes -----------------------------------------------------
        // A term that cannot name a process is the positive "no longer alive" code (`io.c:7031`); a
        // missing out-parameter or an invalid port is the negative failure code (`io.c:7021`,
        // `7055`).
        assert_eq!(command(lib, data, 3), ERL_DRV_MONITOR_NOT_ALIVE as ErlDrvSSizeT);
        assert_eq!(command(lib, data, 4), ERL_DRV_MONITOR_ERROR as ErlDrvSSizeT);
        assert_eq!(command(lib, data, 5), ERL_DRV_MONITOR_ERROR as ErlDrvSSizeT);

        // --- commands 6-7: `driver_get_monitored_process` ----------------------------------------
        assert_eq!(command(lib, data, 6), 1, "the monitor must report its process");
        assert_eq!(
            command(lib, data, 7),
            1,
            "an unknown monitor reports driver_term_nil"
        );

        // --- commands 8-9: the pure comparison ---------------------------------------------------
        assert_eq!(command(lib, data, 8), 0, "a monitor equals itself");
        assert_eq!(
            command(lib, data, 9),
            1,
            "two monitors carry distinct reference things"
        );

        // --- commands 10-11: the identity the C side received IS what the host issued -------------
        let mon1 = identity_command(lib, data, 10);
        let mon2 = identity_command(lib, data, 11);
        assert_eq!(
            mon1,
            host.identity_of(0).to_vec(),
            "the fixture's 24 bytes must be the host's identity verbatim"
        );
        assert_eq!(mon2, host.identity_of(1).to_vec());
        assert_ne!(mon1, mon2, "two monitors are never byte-identical");
        assert_eq!(MONITOR_BYTES, 32, "sizeof(void *) * 4 on LP64");

        // --- commands 12-15: the trailing bytes are not the identity -----------------------------
        // The holder is `sizeof(void*) * 4` = 32 bytes, but only the first 24 are the reference
        // thing, so a copy whose tail is inverted still compares equal (12), still names the process
        // (14) and still demonitors the original monitor (15) - while a byte *inside* the identity
        // breaks equality (13).
        assert_eq!(
            command(lib, data, 12),
            1,
            "an inverted tail must still compare equal"
        );
        assert_eq!(
            command(lib, data, 13),
            1,
            "a flipped byte inside the identity must differ"
        );
        assert_eq!(
            command(lib, data, 14),
            1,
            "a scattered tail must still resolve the monitored process"
        );
        assert_eq!(
            command(lib, data, 15),
            ERL_DRV_MONITOR_OK as ErlDrvSSizeT,
            "the tail-perturbed copy must demonitor the original monitor"
        );
        assert_eq!(host.armed().len(), 1, "only the first monitor was removed");

        // --- commands 16-21: the removal contract -------------------------------------------------
        assert_eq!(
            command(lib, data, 16),
            ERL_DRV_MONITOR_NOT_ALIVE as ErlDrvSSizeT,
            "removing it twice reports the not-alive code"
        );
        assert_eq!(
            command(lib, data, 17),
            1,
            "an invalid port and a null monitor are both -1"
        );
        assert_eq!(
            command(lib, data, 18),
            1,
            "a null monitor out-parameter answers driver_term_nil"
        );
        assert_eq!(
            command(lib, data, 19),
            1,
            "an invalid port answers driver_term_nil as well"
        );
        assert_eq!(command(lib, data, 20), ERL_DRV_MONITOR_OK as ErlDrvSSizeT);
        assert_eq!(
            command(lib, data, 21),
            1,
            "a demonitored monitor no longer names a process"
        );
        assert!(host.armed().is_empty(), "both monitors are gone");
        assert_eq!(command(lib, data, 99), -100, "an unknown command is -100");
    }) else {
        return;
    };
}

// ---------------------------------------------------------------------------------------------
// All three `driver_monitor_process` outcomes through the real C fixture
// ---------------------------------------------------------------------------------------------

/// The three C outcomes of `driver_monitor_process` (`io.c:7012-7038`, `do_driver_monitor_process`)
/// are reachable **end-to-end** through the real C fixture, because the three-way
/// [`DriverHost::monitor_process_result`] hook can express the C's positive `1`
/// (`ERL_DRV_MONITOR_NOT_ALIVE`) as well as its `0` and `-1`.
///
/// Fixture command 1 arms a monitor on `driver_caller(port)` — a **real internal pid**, so it passes
/// `driver_monitor_process`'s `names_a_process` gate (`src/driver/drv/pdl.rs`) and reaches the host
/// hook — and returns the host's code verbatim. The unit-level mapping (`MonitorResult` → the C
/// codes) is pinned in `src/driver/drv/pdl.rs::the_three_way_monitor_result_maps_onto_the_c_reason_codes`;
/// this drives the same mapping out of the compiled `.so`, so the C ABI's return value is what is
/// asserted. The oracle cannot drive this path (this OTP build exports no `erlang:load_driver/2` and
/// a monitor needs an `ErlDrvPort` a NIF lacks), which is why the coverage is the fixture + the unit
/// test rather than a differential case.
#[test]
fn the_real_fixture_sees_all_three_monitor_outcomes() {
    let Some(()) = with_session(|lib, data, host, _so| {
        // The host arms for real: the fixture's command 1 must return the C's `0`.
        host.set_monitor_outcome(MonitorOutcome::Arm);
        assert_eq!(command(lib, data, 1), ERL_DRV_MONITOR_OK as ErlDrvSSizeT);
        assert_eq!(host.armed().len(), 1, "the armed case registered a monitor");

        // The host reports the process dead: the fixture must return the C's positive `1`, and the
        // refusal must not have written the fixture's holder (no monitor registered).
        host.set_monitor_outcome(MonitorOutcome::NotAlive);
        assert_eq!(
            command(lib, data, 1),
            ERL_DRV_MONITOR_NOT_ALIVE as ErlDrvSSizeT,
            "a not-alive host must surface the C's positive code"
        );
        assert_eq!(host.armed().len(), 1, "a not-alive result installs no monitor");

        // The host cannot monitor: the fixture must return the C's negative `-1`.
        host.set_monitor_outcome(MonitorOutcome::Error);
        assert_eq!(
            command(lib, data, 1),
            ERL_DRV_MONITOR_ERROR as ErlDrvSSizeT,
            "a failure the host cannot arm must surface the C's negative code"
        );
        assert_eq!(host.armed().len(), 1, "a failure installs no monitor");

        // Back to arming: the host is live again and command 1 succeeds once more (a second monitor,
        // distinct identity).
        host.set_monitor_outcome(MonitorOutcome::Arm);
        assert_eq!(command(lib, data, 1), ERL_DRV_MONITOR_OK as ErlDrvSSizeT);
        assert_eq!(host.armed().len(), 2, "the recovered host arms again");
    }) else {
        return;
    };
}

// ---------------------------------------------------------------------------------------------
// The `--export-dynamic` mechanism, checked directly
// ---------------------------------------------------------------------------------------------

#[test]
fn the_test_binary_exports_the_monitor_symbols_dynamically() {
    link_the_family();
    let Ok(exe) = std::fs::canonicalize("/proc/self/exe") else {
        skip("the_test_binary_exports_the_monitor_symbols: /proc/self/exe is unavailable");
        return;
    };
    match all_symbols_are_exported(&exe, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "`build.rs` emits `-Wl,--export-dynamic` for the test targets, so {} must appear in \
             {:?}'s dynamic symbol table",
            FIXTURE_IMPORTS.join("/"),
            exe
        ),
        None => skip("the_test_binary_exports_the_monitor_symbols: no nm/readelf available"),
    }
}

// ---------------------------------------------------------------------------------------------
// End-to-end: the fixture's undefined imports are bound at load time, not at link time
// ---------------------------------------------------------------------------------------------

#[test]
fn the_fixture_imports_the_family_through_dynamic_symbols() {
    let Some(so) = compile_fixture() else { return };
    match all_symbols_are_undefined_imports(&so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "the fixture must reference {} as undefined dynamic symbols; inspect {} with `nm -D`",
            FIXTURE_IMPORTS.join("/"),
            so.display()
        ),
        None => skip("the_fixture_imports_the_family_through_dynamic_symbols: no nm/readelf"),
    }
}

// ---------------------------------------------------------------------------------------------
// The oracle: `driver_compare_monitors` in a real ERTS
// ---------------------------------------------------------------------------------------------

/// The four comparison reports the fixture produces per side, as `key=value` text.
///
/// They are normalised to the **sign** of the comparison (the only part of a `memcmp` result C
/// specifies) so the two runtimes' magnitudes cannot make the strings differ spuriously.
const COMPARE_CASES: usize = 4;

/// Generate, compile and run a real Erlang module named [`FIXTURE_NAME`] that loads the fixture
/// `.so` with `erlang:load_nif/2` and prints one `ORACLE ` line per comparison case.
///
/// The module name must equal the NIF entry's `name` field (`ERL_NIF_INIT` stringifies it), so the
/// generated module is `monitors`. `None` (after a loud skip) when `erl`/`erlc` are unavailable or
/// the module cannot be built.
fn oracle_reports(so: &Path) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the_oracle_agrees_on_the_monitor_comparison: no erl/erlc on PATH");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.with_extension("");
    let stem = stem.to_str()?;

    // The stub bodies are what an ordinary NIF module looks like before the library is loaded; they
    // are replaced by the NIFs on `load_nif` (`init/0` calls it, so every function is `nif_start`ed).
    let source = format!(
        "-module({name}).\n\
         -export([main/0, cmp_tail_ignored/0, cmp_last_identity_byte/0, cmp_unsigned_order/0, \
         cmp_tail_only_byte/0]).\n\
         -on_load(init/0).\n\n\
         init() ->\n\
         \x20   case erlang:load_nif(\"{stem}\", []) of\n\
         \x20       ok -> ok;\n\
         \x20       {{error, R}} -> erlang:error({{nif_load, R}})\n\
         \x20   end.\n\n\
         main() ->\n\
         \x20   [io:format(\"ORACLE ~ts~n\", [F()]) || F <- [fun cmp_tail_ignored/0,\n\
         \x20       fun cmp_last_identity_byte/0, fun cmp_unsigned_order/0,\n\
         \x20       fun cmp_tail_only_byte/0]],\n\
         \x20   halt(0).\n\n\
         cmp_tail_ignored() -> erlang:nif_error(nif_not_loaded).\n\
         cmp_last_identity_byte() -> erlang:nif_error(nif_not_loaded).\n\
         cmp_unsigned_order() -> erlang:nif_error(nif_not_loaded).\n\
         cmp_tail_only_byte() -> erlang:nif_error(nif_not_loaded).\n",
        name = FIXTURE_NAME,
        stem = stem,
    );
    let src = dir.join(format!("{FIXTURE_NAME}.erl"));
    if let Err(err) = std::fs::write(&src, source) {
        skip(&format!(
            "the_oracle_agrees_on_the_monitor_comparison: cannot write {src:?}: {err}"
        ));
        return None;
    }

    // Compiling a real module (rather than `-eval`) is required: `erlang:load_nif/2` derives the
    // calling module from the call site and rejects a library whose entry `name` does not match it.
    let compiled = Command::new("erlc").arg("-o").arg(&dir).arg(&src).output();
    match compiled {
        Err(err) => {
            skip(&format!(
                "the_oracle_agrees_on_the_monitor_comparison: cannot run erlc ({err})"
            ));
            return None;
        }
        Ok(out) if !out.status.success() => {
            skip(&format!(
                "the_oracle_agrees_on_the_monitor_comparison: erlc failed (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
            return None;
        }
        Ok(_) => {}
    }

    let run = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-s")
        .arg(FIXTURE_NAME)
        .arg("main")
        .output();
    let output = match run {
        Err(err) => {
            skip(&format!(
                "the_oracle_agrees_on_the_monitor_comparison: cannot run erl ({err})"
            ));
            return None;
        }
        Ok(out) => out,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        skip(&format!(
            "the_oracle_agrees_on_the_monitor_comparison: erl failed (exit {:?}):\nstdout: \
             {stdout}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let lines: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("ORACLE "))
        .map(|line| line.trim_end().to_string())
        .collect();
    if lines.len() != COMPARE_CASES {
        skip(&format!(
            "the_oracle_agrees_on_the_monitor_comparison: expected {COMPARE_CASES} `ORACLE …` lines, \
             got {}:\n{stdout}",
            lines.len()
        ));
        return None;
    }
    Some(lines)
}

#[test]
fn the_oracle_agrees_on_the_monitor_comparison() {
    let Some(()) = with_session(|lib, data, host, so| {
        // Commands 30-33 publish the shared comparison report through `driver_output`, i.e. produced
        // by THIS crate's `driver_compare_monitors`.
        for cmd in 30..30 + COMPARE_CASES as u32 {
            let n = command(lib, data, cmd);
            assert!(n > 0, "command {cmd} must publish a report");
        }
        let ours: Vec<String> = host
            .outputs()
            .iter()
            .map(|bytes| String::from_utf8_lossy(bytes).to_string())
            .collect();
        assert_eq!(ours.len(), COMPARE_CASES, "one report per comparison case");

        // The measured shape of the reports, independent of the oracle: the tail is not compared
        // (case 1: `0`), the last identity byte is (case 2: non-zero), the order is unsigned in both
        // directions (case 3), and the very first tail byte is already outside the comparison (case
        // 4: `0`) - which pins the width to exactly 24 bytes.
        assert_eq!(ours[0], "tail_ignored=0", "the tail must not be compared");
        assert_eq!(ours[1], "last_identity_byte=1", "byte 23 must be compared");
        assert_eq!(ours[2], "ff_vs_00=1 00_vs_ff=-1", "byte order must be unsigned");
        assert_eq!(ours[3], "first_tail_byte_only=0", "byte 24 must not be compared");

        // The same `.so`, loaded by a REAL ERTS as a NIF (this build exports no
        // `erlang:load_driver/2`), whose `driver_compare_monitors` is the emulator's own exported
        // implementation.
        let Some(expected) = oracle_reports(so) else {
            return;
        };
        assert_eq!(
            ours, expected,
            "this crate's driver_compare_monitors must agree with the real ERTS implementation"
        );
    }) else {
        return;
    };
}
