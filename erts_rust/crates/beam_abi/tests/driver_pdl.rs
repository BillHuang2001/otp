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

//! End-to-end proof of the **port-data-lock / driver-monitor / deprecated-wall-clock** family of
//! the driver C ABI (workspace contract **A.4**): the **real C driver**
//! `tests/fixtures/driver_pdl.c` — compiled with the system `cc` against the system
//! `erl_driver.h` — is `dlopen`ed, validated, and driven through the Rust loader, its
//! `driver_*` imports resolving out of the *test binary's* dynamic symbol table.
//!
//! # Why this is the load-bearing test
//!
//! The fixture calls `driver_pdl_create`/`_lock`/`_unlock`/`_get_refc`/`_inc_refc`/`_dec_refc`,
//! `driver_monitor_process`/`driver_demonitor_process`/`driver_get_monitored_process`/
//! `driver_compare_monitors` and `driver_get_now`, whose definitions live in *this* crate
//! (`beam_abi::driver::drv::pdl`). The linker leaves those calls **undefined** in the `.so`
//! (verified below by inspecting its dynamic symbol table), so `dlopen(..., RTLD_NOW)` in
//! [`beam_abi::driver::loader::load`] can only succeed if the *loading process* exports them as
//! dynamic symbols — which `build.rs` arranges with `-Wl,--export-dynamic` on the test targets and
//! the `#[used] DRIVER_API` static keeps in the link. Every value the fixture *returns* was
//! produced by the Rust implementation, so the assertions below pin the Rust behaviour through
//! the C ABI rather than through a Rust-side re-statement of it.
//!
//! # Honest scope of the proof
//!
//! What is proven: a driver `.so` built against the unmodified header loads and its callbacks
//! drive the Rust host functions; the PDL's reference-count sequence, its `NULL`/foreign-handle
//! refusal, the monitor reason codes, the pure monitor comparison and the wall-clock breakdown all
//! behave as the oracle's C does (values cited per command below, from
//! `erts/emulator/beam/io.c`). What is *not* proven here: a real scheduler behind the seam — the
//! test host is a family-local recording `DriverHost`, so "the monitor is really armed on a live
//! process" is a `beam_rt` concern, out of this crate's scope.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`); inspecting symbol tables needs `nm`/`readelf`. When a *toolchain* facility is
//! missing the affected test prints a loud `SKIP:` banner and returns — but when the toolchain
//! **is** present, every assertion is hard (a mismatch fails, it never silently passes).

mod common;

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::loader::{load, DriverLibrary};
use beam_abi::driver::types::{
    ErlDrvData, ErlDrvMonitor, ErlDrvPDL, ErlDrvPort, ErlDrvSSizeT, ErlDrvSInt,
};
use beam_abi::{DriverEnv, DriverHost};
use beam_term::atom::AtomTable;
use beam_term::prelude::{make_atom, make_internal_pid, Eterm};
use libc::{c_char, c_int, c_ulong, c_void};

use common::{compile_so, skip};

/// The committed C fixture — a real driver (see the file header for the A.4 rationale).
const FIXTURE_C: &str = include_str!("fixtures/driver_pdl.c");

/// The driver name the fixture declares (`DRIVER_INIT(pdl)` / `driver_name = "pdl"`).
const FIXTURE_NAME: &str = "pdl";

/// The `driver_*` / `erl_drv_*` symbols the fixture imports (functions **and** the
/// `driver_term_nil` data symbol); they must be *undefined* in the `.so` and *defined* in the
/// loading process.
const FIXTURE_IMPORTS: [&str; 16] = [
    "driver_alloc",
    "driver_free",
    "driver_output",
    "driver_caller",
    "driver_term_nil",
    "driver_pdl_create",
    "driver_pdl_lock",
    "driver_pdl_unlock",
    "driver_pdl_get_refc",
    "driver_pdl_inc_refc",
    "driver_pdl_dec_refc",
    "driver_monitor_process",
    "driver_demonitor_process",
    "driver_get_monitored_process",
    "driver_compare_monitors",
    "driver_get_now",
];

/// The reason codes the family reproduces (`io.c`'s literal returns; the header names none).
const ERL_DRV_MONITOR_OK: c_int = 0;
const ERL_DRV_MONITOR_NOT_ALIVE: c_int = 1;
const ERL_DRV_MONITOR_ERROR: c_int = -1;
/// The count the crate reports for a `NULL`/foreign PDL handle (`ErlDrvSInt`).
const PDL_INVALID_REFCOUNT: ErlDrvSInt = -1;

/// `sizeof(ErlDrvMonitor)` in both the header (`sizeof(void *) * 4`) and this crate.
const MONITOR_BYTES: usize = 4 * std::mem::size_of::<*mut c_void>();

// ---------------------------------------------------------------------------------------------
// Helpers (the same idiom as `tests/driver_end_to_end.rs`)
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
    compile_so("driver_pdl", FIXTURE_C)
}

// ---------------------------------------------------------------------------------------------
// A family-local `DriverHost`: the port's one PDL, a monitor table and the caller pid
// ---------------------------------------------------------------------------------------------

/// A [`DriverHost`] that implements the services this family needs.
///
/// It plays the part of the runtime for the fixture: it owns the port's **single** data lock (the
/// C's `pp->port_data_lock`, so a second `driver_pdl_create` for the same port is refused — the
/// rule the C applies in `io.c:6729`), keeps a monitor table so
/// `demonitor`/`get_monitored_process` can resolve the payload `monitor_process` handed out, and
/// reports a caller pid. It records `output`/`send_term` traffic so a stray call is visible.
struct PdlHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
    /// Every [`DriverHost::output`] byte string, in call order.
    outputs: Vec<Vec<u8>>,
    /// Every [`DriverHost::send_term`] term, in call order.
    sent: Vec<Eterm>,
    /// The live handle storage: only the **address** is ever used as the `ErlDrvPDL`, and the
    /// boxes keep it valid for the whole test (the handle is never dereferenced).
    handles: Vec<Box<u64>>,
    /// The port's data lock, once created.
    pdl: Option<ErlDrvPDL>,
    /// The live monitors: `(payload, monitored process)`.
    monitors: Vec<(ErlDrvMonitor, Eterm)>,
    /// The next monitor payload's id (so two monitors are never byte-identical).
    next_monitor: u64,
    /// What [`DriverHost::caller_pid`] reports.
    caller: Eterm,
}

impl PdlHost {
    fn new() -> PdlHost {
        PdlHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            outputs: Vec::new(),
            sent: Vec::new(),
            handles: Vec::new(),
            pdl: None,
            monitors: Vec::new(),
            next_monitor: 1,
            caller: make_internal_pid(1),
        }
    }

    /// A monitor whose payload encodes `id` in its first word.
    fn monitor(&mut self) -> ErlDrvMonitor {
        let mut data = [0u8; MONITOR_BYTES];
        data[..8].copy_from_slice(&self.next_monitor.to_ne_bytes());
        self.next_monitor += 1;
        ErlDrvMonitor { data }
    }
}

/// The payload bytes behind a monitor pointer (`None` for a null pointer).
///
/// # Safety
///
/// `mon` must be null or point at a readable `ErlDrvMonitor`.
unsafe fn payload_of(mon: *const ErlDrvMonitor) -> Option<&'static [u8]> {
    if mon.is_null() {
        return None;
    }
    // SAFETY: `mon` is non-null and readable for `MONITOR_BYTES` (the caller's contract).
    Some(unsafe { core::slice::from_raw_parts(mon as *const u8, MONITOR_BYTES) })
}

impl DriverHost for PdlHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        self.outputs.push(data.to_vec());
        0
    }

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, term: Eterm) -> c_int {
        self.sent.push(term);
        0
    }

    fn caller_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        Some(self.caller)
    }

    /// One PDL per port, like the C (`io.c:6729`): a live handle means `driver_pdl_create` returns
    /// `NULL`.
    fn pdl_create(&mut self) -> Option<ErlDrvPDL> {
        if self.pdl.is_some() {
            return None;
        }
        let boxed = Box::new(0u64);
        let handle = (&*boxed) as *const u64 as *mut u64 as ErlDrvPDL;
        self.handles.push(boxed);
        self.pdl = Some(handle);
        Some(handle)
    }

    fn monitor_process(&mut self, pid: Eterm) -> Option<ErlDrvMonitor> {
        let mon = self.monitor();
        self.monitors.push((mon, pid));
        Some(mon)
    }

    fn demonitor_process(&mut self, mon: *const ErlDrvMonitor) -> c_int {
        // SAFETY: a `DriverHost` monitor hook is handed a live `ErlDrvMonitor`.
        let Some(payload) = (unsafe { payload_of(mon) }) else {
            return ERL_DRV_MONITOR_ERROR;
        };
        match self.monitors.iter().position(|(m, _)| m.data[..] == *payload) {
            Some(i) => {
                self.monitors.remove(i);
                ERL_DRV_MONITOR_OK
            }
            None => ERL_DRV_MONITOR_NOT_ALIVE,
        }
    }

    fn get_monitored_process(&mut self, mon: *const ErlDrvMonitor) -> Option<Eterm> {
        // SAFETY: a `DriverHost` monitor hook is handed a live `ErlDrvMonitor`.
        let payload = unsafe { payload_of(mon) }?;
        self.monitors
            .iter()
            .find(|(m, _)| m.data[..] == *payload)
            .map(|(_, pid)| *pid)
    }
}

/// Drive one `control` command with no buffers, expecting the fixture to answer.
fn command(lib: &DriverLibrary, data: ErlDrvData, cmd: u32) -> ErlDrvSSizeT {
    let mut reply: *mut c_char = std::ptr::null_mut();
    // SAFETY: `data` is the live state from `start`; the commands driven here read no buffer, and a
    // null reply pointer with `rlen == 0` is what `port_control/3` passes when it wants a length.
    lib.control(data, cmd, std::ptr::null_mut(), 0, &mut reply, 0)
        .expect("the fixture implements control")
}

// ---------------------------------------------------------------------------------------------
// The frozen `driver_*` table
// ---------------------------------------------------------------------------------------------

#[test]
fn the_family_slots_are_populated_and_the_binary_refcount_trio_is_populated_by_mem() {
    // Calling `driver_api_table()` both pins the frozen table shape and forces the
    // `driver_*`-carrying codegen unit into the link, so `--export-dynamic` can publish it.
    let table = driver_api_table();
    for (name, populated) in [
        ("driver_pdl_create", table.driver_pdl_create.is_some()),
        ("driver_pdl_lock", table.driver_pdl_lock.is_some()),
        ("driver_pdl_unlock", table.driver_pdl_unlock.is_some()),
        ("driver_pdl_get_refc", table.driver_pdl_get_refc.is_some()),
        ("driver_pdl_inc_refc", table.driver_pdl_inc_refc.is_some()),
        ("driver_pdl_dec_refc", table.driver_pdl_dec_refc.is_some()),
        ("driver_monitor_process", table.driver_monitor_process.is_some()),
        ("driver_demonitor_process", table.driver_demonitor_process.is_some()),
        (
            "driver_get_monitored_process",
            table.driver_get_monitored_process.is_some(),
        ),
        ("driver_compare_monitors", table.driver_compare_monitors.is_some()),
        ("driver_get_now", table.driver_get_now.is_some()),
        ("driver_caller", table.driver_caller.is_some()),
        ("driver_alloc", table.driver_alloc.is_some()),
        ("driver_free", table.driver_free.is_some()),
        ("driver_output", table.driver_output.is_some()),
        ("driver_mk_term_nil", table.driver_mk_term_nil.is_some()),
    ] {
        assert!(populated, "{name} must be populated");
    }
    // The binary refcount trio is populated by the `mem` family (it shares the driver-binary block
    // layout); it is asserted here too so a break in the `populate` chain is caught from this side.
    assert!(table.driver_binary_get_refc.is_some());
    assert!(table.driver_binary_inc_refc.is_some());
    assert!(table.driver_binary_dec_refc.is_some());
}

// ---------------------------------------------------------------------------------------------
// The `--export-dynamic` mechanism, checked directly
// ---------------------------------------------------------------------------------------------

#[test]
fn the_test_binary_exports_the_pdl_and_monitor_symbols_dynamically() {
    // Force the family's definitions into the link before inspecting it.
    assert!(driver_api_table().driver_pdl_create.is_some());
    assert!(driver_api_table().driver_monitor_process.is_some());
    assert!(driver_api_table().driver_get_now.is_some());

    let Ok(exe) = std::fs::canonicalize("/proc/self/exe") else {
        skip("the_test_binary_exports_the_pdl_and_monitor_symbols: /proc/self/exe is unavailable");
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
        None => skip("the_test_binary_exports_the_pdl_and_monitor_symbols: no nm/readelf available"),
    }
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load the real driver `.so` and drive the family through the C ABI
// ---------------------------------------------------------------------------------------------

#[test]
fn loads_and_drives_a_real_c_driver_through_the_pdl_family() {
    // Load-bearing: this pins the frozen `driver_*` table AND keeps the family's codegen unit in
    // the link (without it the `.so`'s imports would not resolve and `dlopen` would fail).
    assert!(driver_api_table().driver_pdl_create.is_some());

    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    // --- the `.so` really imports this family (bound at load time, not at link time) ------------
    match all_symbols_are_undefined_imports(&so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "the fixture must reference {} as undefined dynamic symbols; inspect {} with `nm -D`",
            FIXTURE_IMPORTS.join("/"),
            so.display()
        ),
        None => skip("loads_and_drives_a_real_c_driver_through_the_pdl_family: no nm/readelf"),
    }

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ----------------------------------------
    let lib = load(&path, FIXTURE_NAME)
        .expect("a real driver must load: its driver_* imports resolve from the test binary");
    assert_eq!(lib.name(), FIXTURE_NAME);

    // --- the port/state and the commands --------------------------------------------------------
    let mut host = PdlHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command_line = CString::new("start").expect("no interior NUL");

    // `start` allocates its private state and hands it back verbatim (never a failure code).
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; the command is NUL-ended.
    let data = lib
        .start(port, command_line.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null(), "start must return the private state, not a failure code");

    // --- command 0: the documented no-op --------------------------------------------------------
    assert_eq!(command(&lib, data, 0), 0);

    // --- commands 1-3: `driver_pdl_create` ------------------------------------------------------
    // A valid port admits one PDL; a second create for the same port returns NULL (`io.c:6729`);
    // an invalid port returns NULL (`ERTS_INVALID_ERL_DRV_PORT`, `io.c:6720`).
    assert_eq!(command(&lib, data, 1), 1, "a valid port must admit a data lock");
    assert_eq!(command(&lib, data, 2), 1, "a second create for the port must return NULL");
    assert_eq!(command(&lib, data, 3), 1, "an invalid port must return NULL");

    // --- commands 4-6: the reference count ------------------------------------------------------
    // `pdl_init_refc` sets 1 (`io.c:6726`); `inc_refc`/`dec_refc` return the value reached.
    assert_eq!(command(&lib, data, 4), 1, "a fresh PDL has exactly one reference");
    assert_eq!(command(&lib, data, 5), 2, "inc_refc returns the value reached");
    assert_eq!(command(&lib, data, 6), 1, "dec_refc returns the value reached");

    // --- commands 7-8: lock/unlock raise then lower the count -----------------------------------
    // `driver_pdl_lock` is `pdl_inc_refc` **then** `erts_mtx_lock` (`io.c:6736`), so the count
    // reads one higher inside the critical section; `unlock` is the mirror (`io.c:6746`).
    assert_eq!(command(&lib, data, 7), 2, "the count is raised before the lock is taken");
    assert_eq!(command(&lib, data, 8), 1, "the count is lowered after the lock is released");

    // --- commands 9-10: bad handles are refused, never dereferenced -----------------------------
    // The C dereferences the handle; this runtime reports `PDL_INVALID_REFCOUNT` (-1) for a handle
    // it did not issue (a reference count is never negative), and lock/unlock of one is a no-op.
    assert_eq!(command(&lib, data, 9) as ErlDrvSInt, PDL_INVALID_REFCOUNT);
    assert_eq!(command(&lib, data, 10) as ErlDrvSInt, PDL_INVALID_REFCOUNT);

    // --- commands 11-15: `driver_monitor_process` -----------------------------------------------
    // Success is 0 with the monitor filled (`io.c:7037`); an operand that cannot name a process is
    // the positive "no longer alive" code (`io.c:7031`); a missing out-parameter or an invalid
    // port is the negative failure code (`io.c:7021`, `io.c:7055`).
    assert_eq!(command(&lib, data, 11), ERL_DRV_MONITOR_OK as ErlDrvSSizeT);
    assert_eq!(command(&lib, data, 12), ERL_DRV_MONITOR_OK as ErlDrvSSizeT);
    assert_eq!(
        command(&lib, data, 13),
        ERL_DRV_MONITOR_NOT_ALIVE as ErlDrvSSizeT,
        "a term that cannot name a process is not alive"
    );
    assert_eq!(command(&lib, data, 14), ERL_DRV_MONITOR_ERROR as ErlDrvSSizeT);
    assert_eq!(command(&lib, data, 15), ERL_DRV_MONITOR_ERROR as ErlDrvSSizeT);
    assert_eq!(host.monitors.len(), 2, "the two monitors must be registered on the host");

    // --- commands 16-19: `driver_get_monitored_process` / `driver_compare_monitors` -------------
    // The monitor reports the process it was made for; an unknown monitor reports
    // `driver_term_nil` (`io.c:7113`); `compare` is a pure `memcmp` of the reference things
    // (`io.c:7142`), so a monitor equals itself and two distinct monitors differ.
    assert_eq!(command(&lib, data, 16), 1, "the monitor must report its process");
    assert_eq!(command(&lib, data, 17), 1, "an unknown monitor reports driver_term_nil");
    assert_eq!(command(&lib, data, 18), 0, "a monitor compares equal to itself");
    assert_eq!(command(&lib, data, 19), 1, "two monitors carry distinct reference things");
    assert_eq!(MONITOR_BYTES, 32, "sizeof(void *) * 4 on LP64");

    // --- commands 20-22: `driver_demonitor_process` ---------------------------------------------
    // `0` when a monitor was removed, `1` when it no longer exists (`io.c:7074-7080`), `-1` for an
    // invalid port or a missing monitor (`io.c:7069`, and this runtime's null-pointer refusal).
    assert_eq!(command(&lib, data, 20), ERL_DRV_MONITOR_OK as ErlDrvSSizeT);
    assert_eq!(command(&lib, data, 21), ERL_DRV_MONITOR_NOT_ALIVE as ErlDrvSSizeT);
    assert_eq!(command(&lib, data, 22), 1, "an invalid port and a null monitor are -1");
    assert_eq!(host.monitors.len(), 1, "only the second monitor was removed");

    // --- command 23: `driver_get_now` returns the 24-byte three-field breakdown -----------------
    let mut reply = vec![0u8; 3 * std::mem::size_of::<c_ulong>()];
    let mut reply_ptr = reply.as_mut_ptr() as *mut c_char;
    // SAFETY: `data` is live; `reply_ptr` points at 24 writable bytes and `rlen` reports that.
    let n = lib
        .control(
            data,
            23,
            std::ptr::null_mut(),
            0,
            &mut reply_ptr,
            reply.len(),
        )
        .expect("the fixture implements control");
    assert_eq!(
        n as usize,
        reply.len(),
        "driver_get_now must fill the whole ErlDrvNowData"
    );
    let word = 8usize;
    let field = |i: usize| c_ulong::from_ne_bytes(reply[i * word..(i + 1) * word].try_into().unwrap());
    let (megasecs, secs, microsecs) = (field(0), field(1), field(2));
    assert!(secs < 1_000_000, "secs is a within-megasecond remainder: {secs}");
    assert!(
        microsecs < 1_000_000,
        "microsecs is a within-second remainder: {microsecs}"
    );
    // The breakdown must reconstruct a wall clock close to `SystemTime::now()` (the `ErlDrvNowData`
    // epoch is documented as arbitrary; this runtime anchors it on the Unix epoch).
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock is after the epoch")
        .as_secs();
    let total = megasecs as u64 * 1_000_000 + secs as u64;
    assert!(
        total + 2 >= unix && total <= unix + 2,
        "the breakdown ({total}s) must be within 2s of SystemTime ({unix}s)"
    );

    // --- command 24 + the default: `driver_get_now(NULL)` is -1; an unknown command is -100 ------
    assert_eq!(command(&lib, data, 24), -1, "a NULL out-parameter is the C's -1");
    assert_eq!(command(&lib, data, 99), -100, "an unknown command is visible as -100");

    // The fixture never wrote to the port except through the reply buffer, and the PDL is back to
    // its initial single reference after every lock was matched by an unlock.
    assert!(host.outputs.is_empty(), "no command should have produced port output");
    assert!(host.sent.is_empty(), "no command should have sent a term");
    assert_eq!(command(&lib, data, 4), 1, "every lock was matched by an unlock");

    // `stop` frees the private state (no leak, no double free: it is called exactly once here).
    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    lib.stop(data).expect("the fixture implements stop");
    drop(lib);
}
