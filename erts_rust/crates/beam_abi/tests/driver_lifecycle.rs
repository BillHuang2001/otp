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

//! End-to-end proof of the **port-lifecycle, failure and attributes** family of the driver C ABI
//! (workspace contract **A.4**): the **real C driver** `tests/fixtures/driver_lifecycle.c` —
//! compiled with the system `cc` against the system `erl_driver.h` — is `dlopen`ed, validated, and
//! driven through the Rust loader, its `driver_*` / `erl_drv_*` imports resolving out of the *test
//! binary's* dynamic symbol table.
//!
//! # Why this is the load-bearing test
//!
//! The fixture calls `set_busy_port`, `set_port_control_flags`/`get_port_flags`,
//! `erl_drv_busy_msgq_limits`, `erl_drv_init_ack`, `erl_drv_set_os_pid`,
//! `driver_async_port_key` and the whole `driver_failure*`/`driver_exit` family, whose definitions
//! live in *this* crate (`beam_abi::driver::drv::lifecycle`). The linker leaves those calls
//! **undefined** in the `.so` (verified below by inspecting its dynamic symbol table), so
//! `dlopen(..., RTLD_NOW)` in [`beam_abi::driver::loader::load`] can only succeed if the *loading
//! process* exports them as dynamic symbols — which `build.rs` arranges with
//! `-Wl,-export-dynamic` on the test targets and the `#[used] DRIVER_API` static keeps in the link.
//! Every value the fixture *returns* was therefore produced by the Rust implementation, so the
//! assertions below pin the Rust behaviour through the C ABI rather than through a Rust-side
//! restatement of it.
//!
//! # Honest scope of the proof
//!
//! What is proven: a driver `.so` built against the unmodified header loads and its callbacks drive
//! the Rust host functions; the C's return values for a valid port (`0`), an invalid port (`-1`,
//! or `0` for `get_port_flags`/`driver_async_port_key`) and the `void` entry points' silent no-op
//! are reproduced; each `driver_failure*` spelling lands on the host's single `failure` hook with
//! its own reason; a failed `erl_drv_init_ack` result is never acknowledged (it closes the port, as
//! the C does); and the `set_port_control_flags` → `get_port_flags` values round-trip.
//! What is *not* proven here: a real port table / scheduler behind the seam — the test host is a
//! family-local recording [`DriverHost`], so "the port is really closed in a live emulator" is a
//! `beam_rt` concern, out of this crate's scope.
//!
//! This file additionally drives the four **driver-registry / port-creation** slots (`registry.rs`:
//! `add_driver_entry`, `remove_driver_entry`, `driver_lock_driver`, `driver_create_port`) through the
//! frozen table's *function pointers* — with the same recording host and the scoped
//! `with_current_host` publication the two entry-list slots need — because those slots reach the
//! runtime's process-global state rather than one port's term services.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`); inspecting symbol tables needs `nm`/`readelf`. When a *toolchain* facility is
//! missing the affected test prints a loud `SKIP:` banner and returns — but when the toolchain
//! **is** present, every assertion is hard (a mismatch fails, it never silently passes).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::host::PortFailure;
use beam_abi::driver::loader::{load, DriverLibrary};
use beam_abi::driver::types::{
    ErlDrvData, ErlDrvEntry, ErlDrvPort, ErlDrvSizeT, ErlDrvTermData, ERTS_INVALID_ERL_DRV_PORT,
};
use beam_abi::{with_current_host, DriverEnv, DriverHost};
use beam_term::atom::AtomTable;
use beam_term::prelude::{make_atom, Eterm};
use libc::{c_char, c_int, c_uint};

use common::{compile_so, skip};

/// The committed C fixture — a real driver (see the file header for the A.4 rationale).
const FIXTURE_C: &str = include_str!("fixtures/driver_lifecycle.c");

/// The driver name the fixture declares (`DRIVER_INIT(lifecycle)` / `driver_name = "lifecycle"`).
const FIXTURE_NAME: &str = "lifecycle";

/// The `driver_*` / `erl_drv_*` symbols the fixture imports. They must be *undefined* in the `.so`
/// and *defined* in the loading process — this is the whole mechanism, so it is checked directly.
const FIXTURE_IMPORTS: [&str; 14] = [
    "driver_alloc",
    "driver_free",
    "set_busy_port",
    "set_port_control_flags",
    "get_port_flags",
    "erl_drv_busy_msgq_limits",
    "erl_drv_init_ack",
    "erl_drv_set_os_pid",
    "driver_async_port_key",
    "driver_failure",
    "driver_failure_atom",
    "driver_failure_eof",
    "driver_failure_posix",
    "driver_exit",
];

/// `PORT_CONTROL_FLAG_BINARY` / `PORT_FLAG_BINARY` (`erl_driver.h:98`, `erl_driver.h:103`).
const CONTROL_FLAG_BINARY: c_int = 1 << 0;
/// `PORT_CONTROL_FLAG_HEAVY` / `PORT_FLAG_LINE` (`erl_driver.h:99`, `erl_driver.h:104`).
const CONTROL_FLAG_HEAVY: c_int = 1 << 1;
/// `ERL_DRV_BUSY_MSGQ_DISABLED` (`erl_driver.h:323`).
const BUSY_MSGQ_DISABLED: ErlDrvSizeT = !0;
/// `ERL_DRV_ERROR_BADARG` as the `ErlDrvData` the fixture acks with (`erl_driver.h:190`).
const ERROR_BADARG: ErlDrvData = -3isize as ErlDrvData;

// ---------------------------------------------------------------------------------------------
// Helpers (the same idiom as `tests/driver_pdl.rs` / `tests/driver_end_to_end.rs`)
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
    compile_so("driver_lifecycle", FIXTURE_C)
}

/// Drive one `control` command that needs no buffers, expecting the fixture to answer.
fn command(lib: &DriverLibrary, data: ErlDrvData, cmd: u32) -> c_int {
    let mut reply: *mut c_char = std::ptr::null_mut();
    // SAFETY: `data` is the live state from `start`; the commands driven here read no buffer, and a
    // null reply pointer with `rlen == 0` is what `port_control/3` passes when it wants a length.
    lib.control(data, cmd, std::ptr::null_mut(), 0, &mut reply, 0)
        .expect("the fixture implements control") as c_int
}

// ---------------------------------------------------------------------------------------------
// A family-local recording `DriverHost`
// ---------------------------------------------------------------------------------------------

/// A [`DriverHost`] that records every service this family drives through the seam.
///
/// `tests/common/mod.rs`'s `TestDriverHost` deliberately covers only the output/term/select/timer
/// surface, so this family brings its own host (the same choice `tests/driver_pdl.rs` made). It
/// plays the part of the runtime: `set_control_flags` is the one hook whose effect the C ABI can
/// read back (`get_port_flags`), so the round trip is observable end-to-end; every other hook
/// records the request.
struct LifecycleTestHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
    /// Every `failure` reason, in call order (`PortFailure`'s `Debug` spelling).
    failures: Vec<String>,
    /// Every `port_exit` code.
    exits: Vec<c_int>,
    /// Every `set_busy_port` request.
    busy: Vec<bool>,
    /// Every `set_control_flags` request as `(mode, flags)`.
    control: Vec<(c_int, c_int)>,
    /// What `get_port_flags` reports (the last flags `set_control_flags` was given).
    flags: Option<c_int>,
    /// Every `busy_msgq_limits` request as `(low, high)`.
    limits: Vec<(c_int, c_int)>,
    /// The number of `init_ack` calls.
    acks: usize,
    /// Every `set_os_pid` request.
    os_pids: Vec<c_int>,
    /// The key `async_port_key` writes.
    key: c_uint,
    /// Every `add_driver_entry` operand (the `registry` family).
    added: Vec<*mut ErlDrvEntry>,
    /// Every `remove_driver_entry` operand.
    removed: Vec<*mut ErlDrvEntry>,
    /// Every `lock_driver` port operand.
    locked: Vec<ErlDrvPort>,
    /// Every `create_port` request as `(creator, connected, name, drv_data)`.
    created: Vec<(ErlDrvPort, ErlDrvTermData, *mut c_char, ErlDrvData)>,
    /// The handle `create_port` hands back (a distinctive non-null value).
    created_port: ErlDrvPort,
}

impl LifecycleTestHost {
    fn new() -> LifecycleTestHost {
        LifecycleTestHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            failures: Vec::new(),
            exits: Vec::new(),
            busy: Vec::new(),
            control: Vec::new(),
            flags: None,
            limits: Vec::new(),
            acks: 0,
            os_pids: Vec::new(),
            key: 0x4c_1f_e,
            added: Vec::new(),
            removed: Vec::new(),
            locked: Vec::new(),
            created: Vec::new(),
            created_port: 0x5a5a_isize as ErlDrvPort,
        }
    }
}

impl DriverHost for LifecycleTestHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
        0
    }

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
        0
    }

    fn set_busy_port(&mut self, busy: bool) -> c_int {
        self.busy.push(busy);
        0
    }

    /// The port's control flags are readable again through [`Self::get_port_flags`] — the seam
    /// contract that makes the C ABI's `set_port_control_flags` → `get_port_flags` round trip
    /// observable (see `lifecycle.rs`'s deviations: the C keeps the two in *different* fields).
    fn set_control_flags(&mut self, mode: c_int, flags: c_int) -> c_int {
        self.control.push((mode, flags));
        self.flags = Some(flags);
        0
    }

    fn get_port_flags(&mut self) -> Option<c_int> {
        self.flags
    }

    fn failure(&mut self, reason: PortFailure<'_>) -> c_int {
        self.failures.push(format!("{reason:?}"));
        0
    }

    fn port_exit(&mut self, code: c_int) -> c_int {
        self.exits.push(code);
        0
    }

    fn busy_msgq_limits(&mut self, low: c_int, high: c_int) -> c_int {
        self.limits.push((low, high));
        0
    }

    fn init_ack(&mut self) -> c_int {
        self.acks += 1;
        0
    }

    fn set_os_pid(&mut self, pid: c_int) -> c_int {
        self.os_pids.push(pid);
        0
    }

    fn async_port_key(&mut self, key: *mut c_uint) -> c_int {
        if key.is_null() {
            return -1;
        }
        // SAFETY: the hook's out-parameter contract is a writable `c_uint`.
        unsafe { *key = self.key };
        0
    }

    fn add_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        self.added.push(de);
        0
    }

    fn remove_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        self.removed.push(de);
        1
    }

    fn lock_driver(&mut self, port: ErlDrvPort) -> c_int {
        self.locked.push(port);
        0
    }

    fn create_port(
        &mut self,
        creator: ErlDrvPort,
        connected: ErlDrvTermData,
        name: *mut c_char,
        drv_data: ErlDrvData,
    ) -> ErlDrvPort {
        self.created.push((creator, connected, name, drv_data));
        self.created_port
    }
}

// ---------------------------------------------------------------------------------------------
// The header constants the fixture's `switch` is written against
// ---------------------------------------------------------------------------------------------

#[test]
fn the_header_constants_the_fixture_assumes_equal_the_crates() {
    use beam_abi::driver::types::{ERL_DRV_BUSY_MSGQ_DISABLED, ERL_DRV_ERROR_BADARG};

    // The fixture's C expressions (`PORT_CONTROL_FLAG_*`, `PORT_FLAG_*`, `ENOENT`,
    // `ERL_DRV_BUSY_MSGQ_DISABLED`, `ERL_DRV_ERROR_BADARG`) come from the *system* header; these
    // three are the ones the Rust side also spells, so a drift would make the assertions above
    // wrong rather than merely different. `PORT_CONTROL_FLAG_BINARY == PORT_FLAG_BINARY` and
    // `PORT_CONTROL_FLAG_HEAVY == PORT_FLAG_LINE` are both true in the header, which is why the
    // round trip in commands 2-3 can compare the two constant families.
    assert_eq!(BUSY_MSGQ_DISABLED, ERL_DRV_BUSY_MSGQ_DISABLED);
    assert_eq!(ERROR_BADARG, ERL_DRV_ERROR_BADARG);
    assert_eq!(CONTROL_FLAG_BINARY, 1);
    assert_eq!(CONTROL_FLAG_HEAVY, 2);
}

// ---------------------------------------------------------------------------------------------
// The frozen `driver_*` table
// ---------------------------------------------------------------------------------------------

#[test]
fn the_family_slots_are_populated_in_the_frozen_table() {
    // Calling `driver_api_table()` both pins the frozen table shape and forces the
    // `driver_*`-carrying codegen unit into the link, so `--export-dynamic` can publish it.
    let table = driver_api_table();
    for (name, populated) in [
        ("erl_drv_busy_msgq_limits", table.erl_drv_busy_msgq_limits.is_some()),
        ("driver_failure_eof", table.driver_failure_eof.is_some()),
        ("driver_failure_atom", table.driver_failure_atom.is_some()),
        ("driver_failure_posix", table.driver_failure_posix.is_some()),
        ("driver_failure", table.driver_failure.is_some()),
        ("driver_exit", table.driver_exit.is_some()),
        ("set_busy_port", table.set_busy_port.is_some()),
        ("set_port_control_flags", table.set_port_control_flags.is_some()),
        ("get_port_flags", table.get_port_flags.is_some()),
        ("erl_drv_init_ack", table.erl_drv_init_ack.is_some()),
        ("erl_drv_set_os_pid", table.erl_drv_set_os_pid.is_some()),
        ("driver_async_port_key", table.driver_async_port_key.is_some()),
    ] {
        assert!(populated, "{name} must be populated");
    }
}

// ---------------------------------------------------------------------------------------------
// The driver-registry / port-creation family (the `registry` file), driven through the table
// ---------------------------------------------------------------------------------------------

/// The invalid-port sentinel (`ERTS_INVALID_ERL_DRV_PORT`, `erts/emulator/beam/erl_port.h:67`).
const INVALID_PORT: isize = -1;

#[test]
fn the_registry_slots_are_populated_in_the_frozen_table() {
    let table = driver_api_table();
    for (name, populated) in [
        ("add_driver_entry", table.add_driver_entry.is_some()),
        ("remove_driver_entry", table.remove_driver_entry.is_some()),
        ("driver_lock_driver", table.driver_lock_driver.is_some()),
        ("driver_create_port", table.driver_create_port.is_some()),
    ] {
        assert!(populated, "{name} must be populated");
    }
}

/// The four slots driven through the frozen table's *function pointers* — the ABI path a driver
/// takes — with a real recording host behind them.
#[test]
fn the_registry_slots_drive_the_host_through_the_frozen_table() {
    let table = driver_api_table();
    let mut host = LifecycleTestHost::new();

    // A dangling `ErlDrvEntry*`: the entry-list slots only forward it, so any dereference would be a
    // use-after-free the test would fault on rather than pass.
    let de = core::ptr::NonNull::<ErlDrvEntry>::dangling().as_ptr();
    let add = table.add_driver_entry.expect("populated");
    let remove = table.remove_driver_entry.expect("populated");

    // The two entry-list slots carry no port operand, so the host comes from the scoped publication.
    with_current_host(&mut host, || {
        // SAFETY: the slots forward the entry pointer to the host; it is never dereferenced here.
        unsafe {
            add(de); // the C's `void` wrapper: its result is deliberately discarded
            assert_eq!(remove(de), 1, "'1' = removed");
        }
    });
    assert_eq!(host.added, vec![de]);
    assert_eq!(host.removed, vec![de]);

    // Outside any publication the slots answer the C's values without touching the operand.
    // SAFETY: as above.
    assert_eq!(unsafe { remove(de) }, 0, "no current host => `0` (no such entry)");

    // The two port-carrying slots recover the port's own env.
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let name = c"lifecycle";
    let lock = table.driver_lock_driver.expect("populated");
    let create = table.driver_create_port.expect("populated");
    // SAFETY: `port` came from `DriverEnv::as_port` on a live env; `name` is NUL-terminated.
    let created = unsafe { create(port, 7, name.as_ptr() as *mut c_char, core::ptr::null_mut()) };
    // SAFETY: `port` is live.
    let locked = unsafe { lock(port) };
    assert_eq!(locked, 0, "the host locked the port's own driver");
    assert_eq!(created, host.created_port);
    assert_eq!(host.locked, vec![port]);
    assert_eq!(host.created.len(), 1);
    assert_eq!(host.created[0].0, port);

    // A null creator port is the sentinel, never a dereference (`erl_port.h:67`).
    // SAFETY: a null port is `env_for`'s documented `None`; `name` is NUL-terminated.
    unsafe {
        assert_eq!(lock(core::ptr::null_mut()), -1);
        assert_eq!(
            create(
                core::ptr::null_mut(),
                7,
                name.as_ptr() as *mut c_char,
                core::ptr::null_mut()
            ) as isize,
            INVALID_PORT
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The `--export-dynamic` mechanism, checked directly
// ---------------------------------------------------------------------------------------------

#[test]
fn the_test_binary_exports_the_lifecycle_symbols_dynamically() {
    // Force the family's definitions into the link before inspecting it.
    assert!(driver_api_table().driver_failure.is_some());
    assert!(driver_api_table().get_port_flags.is_some());
    assert!(driver_api_table().erl_drv_busy_msgq_limits.is_some());

    let Ok(exe) = std::fs::canonicalize("/proc/self/exe") else {
        skip("the_test_binary_exports_the_lifecycle_symbols_dynamically: /proc/self/exe unavailable");
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
        None => skip(
            "the_test_binary_exports_the_lifecycle_symbols_dynamically: no nm/readelf available",
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load the real driver `.so` and drive the family through the C ABI
// ---------------------------------------------------------------------------------------------

#[test]
fn loads_and_drives_a_real_c_driver_through_the_lifecycle_family() {
    // Load-bearing: this pins the frozen `driver_*` table AND keeps the family's codegen unit in
    // the link (without it the `.so`'s imports would not resolve and `dlopen` would fail).
    assert!(driver_api_table().driver_failure.is_some());

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
        None => skip("loads_and_drives_a_real_c_driver_through_the_lifecycle_family: no nm/readelf"),
    }

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ----------------------------------------
    let lib = load(&path, FIXTURE_NAME)
        .expect("a real driver must load: its driver_* imports resolve from the test binary");
    assert_eq!(lib.name(), FIXTURE_NAME);

    // --- the port and the private state ---------------------------------------------------------
    let mut host = LifecycleTestHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command_line = std::ffi::CString::new("start").expect("no interior NUL");

    // `start` allocates its private state and hands it back verbatim (never a failure code).
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; the command is NUL-ended.
    let data = lib
        .start(port, command_line.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null(), "start must return the private state, not a failure code");

    assert_eq!(command(&lib, data, 0), 0, "the documented no-op");

    // --- commands 1: `set_busy_port` (void) -----------------------------------------------------
    assert_eq!(command(&lib, data, 1), 0);
    assert_eq!(host.busy, [true, false], "both requests must reach the host, in order");

    // --- commands 2-4: the control-flags round trip ---------------------------------------------
    assert_eq!(command(&lib, data, 2), 1, "PORT_CONTROL_FLAG_BINARY must read back as PORT_FLAG_BINARY");
    assert_eq!(command(&lib, data, 3), 1, "PORT_CONTROL_FLAG_HEAVY must read back as PORT_FLAG_LINE");
    assert_eq!(command(&lib, data, 4), 1, "clearing the flags must read back as none");
    assert_eq!(
        host.control,
        [
            (0, CONTROL_FLAG_BINARY),
            (0, CONTROL_FLAG_HEAVY),
            (0, 0),
        ],
        "the frozen C function has one `flags` operand, so the seam's `mode` operand is 0"
    );
    assert_eq!(host.flags, Some(0), "the last value set was 0");

    // --- commands 5-8: `erl_drv_busy_msgq_limits` -----------------------------------------------
    assert_eq!(command(&lib, data, 5), 1, "a write-read request is echoed back verbatim");
    assert_eq!(
        command(&lib, data, 6),
        1,
        "ERL_DRV_BUSY_MSGQ_DISABLED must be reported back for both bounds"
    );
    assert_eq!(
        command(&lib, data, 7),
        1,
        "an invalid port reports ERL_DRV_BUSY_MSGQ_DISABLED for both bounds"
    );
    assert_eq!(command(&lib, data, 8), 1, "an absent bound pointer must not be written through");
    assert_eq!(
        host.limits,
        [(2, 8), (-1, -1), (0, 3)],
        "the disable sentinel and the `DISABLED` watermark travel as the seam's -1; the null-port \
         call and the absent-`low` call must not be visible as requests of their own"
    );

    // --- commands 9-11: `erl_drv_init_ack` ------------------------------------------------------
    assert_eq!(command(&lib, data, 9), 0);
    assert_eq!(command(&lib, data, 10), 0);
    assert_eq!(command(&lib, data, 11), 0);
    assert_eq!(
        host.acks, 2,
        "a pointer result and NULL are the C's success replies; ERL_DRV_ERROR_BADARG is not"
    );
    assert_eq!(
        host.exits,
        [0],
        "a failed init closes the port with `normal` — the C's driver_failure_term(ix, am_normal, 0)"
    );

    // --- commands 12-14: `erl_drv_set_os_pid` / `driver_async_port_key` -------------------------
    assert_eq!(command(&lib, data, 12), 0);
    assert_eq!(host.os_pids, [4242]);
    assert_eq!(command(&lib, data, 13), 1, "a resolvable port has a non-zero async key");
    assert_eq!(command(&lib, data, 14), 1, "an invalid port has no async key");

    // --- commands 15-16: the `driver_failure*` family / `driver_exit` returns --------------------
    assert_eq!(
        command(&lib, data, 15),
        1,
        "every driver_failure* spelling must return 0 for a valid port"
    );
    assert_eq!(command(&lib, data, 16), 1, "driver_exit returns 0 for a valid port");
    // `driver_failure(st->port, 0)` is the C's **eof route** (`driver_failure_term(ix,
    // make_small(0), code == 0)`, `io.c:7258`): the reason term is discarded, so the port owner
    // observes the measured reason `normal` — identical to `driver_failure_eof`, and hence `Eof` on
    // the seam, never `Error(0)`.
    assert_eq!(
        host.failures,
        [
            "Eof".to_string(),
            "Atom(\"enoent\")".to_string(),
            format!("Posix({})", libc::ENOENT),
            "Eof".to_string(),
            "Error(7)".to_string(),
        ],
        "each C entry point must land on the one `failure` hook with its own reason, in order"
    );
    assert_eq!(
        host.exits,
        [0, 0, 3],
        "the two driver_exit codes reach `port_exit` verbatim, after the init-ack close"
    );

    // --- commands 17-18: invalid ports ----------------------------------------------------------
    assert_eq!(
        command(&lib, data, 17),
        1,
        "an invalid port must give the C's failure values, never a crash"
    );
    assert_eq!(command(&lib, data, 18), 1, "the void entry points are silent no-ops for NULL");
    assert_eq!(host.failures.len(), 5, "the invalid-port failures must record nothing");
    assert_eq!(host.exits.len(), 3);
    assert!(host.busy.len() == 2, "the invalid-port set_busy_port must record nothing");
    assert_eq!(host.limits.len(), 3);
    assert_eq!(host.acks, 2);
    assert_eq!(host.os_pids, [4242], "the invalid-port set_os_pid must record nothing");

    // --- an unknown command is visible as a distinct negative marker -----------------------------
    assert_eq!(command(&lib, data, 99), -100);

    // `stop` frees the private state (no leak, no double free: it is called exactly once here).
    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    lib.stop(data).expect("the fixture implements stop");
    drop(lib);
}

// ---------------------------------------------------------------------------------------------
// The one slot this family drives with a *failure* result of its own hook
// ---------------------------------------------------------------------------------------------

#[test]
fn a_host_without_the_hooks_still_survives_the_whole_fixture() {
    // The `DriverHost` *defaults* report "unsupported" (`-1`/`None`), which is the state a future
    // `beam_rt` port without these services is in. The C ABI must then behave exactly as documented
    // — no panic, no fabricated success — so every command is replayed against a bare host.
    struct BareHost {
        heap: beam_alloc::Heap,
    }

    impl DriverHost for BareHost {
        fn heap(&mut self) -> &mut beam_alloc::Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, _name: &str) -> Option<Eterm> {
            None
        }

        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            -1
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            -1
        }
    }

    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let lib = load(&path, FIXTURE_NAME).expect("a real driver must load");

    let mut host = BareHost {
        heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
    };
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command_line = std::ffi::CString::new("start").expect("no interior NUL");
    // SAFETY: `port` is live and the command is NUL-ended.
    let data = lib
        .start(port, command_line.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null());

    // The commands whose value is the *host's* code now report "unsupported"…
    assert_eq!(command(&lib, data, 3), 0, "get_port_flags(None) is the C's 0");
    assert_eq!(command(&lib, data, 5), 0, "busy_msgq_limits(-1) reports DISABLED, not the request");
    assert_eq!(command(&lib, data, 11), 0);
    assert_eq!(command(&lib, data, 13), 0, "an unsupported async_port_key has no key");
    // …while the commands whose value is the *C's* own constant are unchanged: an invalid port's
    // answers and the `0`/`-1` returns do not depend on the host at all.
    assert_eq!(command(&lib, data, 6), 1, "the DISABLED echo needs no host feature");
    assert_eq!(command(&lib, data, 7), 1, "the invalid-port DISABLED answer needs no hook");
    assert_eq!(command(&lib, data, 14), 1);
    assert_eq!(
        command(&lib, data, 15),
        0,
        "every driver_failure* now reports the host's -1, exactly as a driver following the \
         documented `< 0` idiom expects"
    );
    assert_eq!(command(&lib, data, 16), 0, "driver_exit reports the host's -1 as well");
    assert_eq!(command(&lib, data, 17), 1, "the invalid-port failures are host-independent");
    assert_eq!(command(&lib, data, 18), 1, "the NULL entry points are silent no-ops");

    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    lib.stop(data).expect("the fixture implements stop");
    drop(lib);
}

// ---------------------------------------------------------------------------------------------
// The eof route and the invalid-port operand, driven through the frozen table's function pointers
// ---------------------------------------------------------------------------------------------

/// `driver_failure(P, 0)` and `driver_failure_eof(P)` are the **same request** to the C:
/// `driver_failure(ix, code)` is `driver_failure_term(ix, make_small(code), code == 0)`
/// (`io.c:7258`), and the `eof` branch discards the reason term
/// (`erts_deliver_port_exit(prt, id, eof ? am_normal : term, …)`), so the port owner observes the
/// reason `normal` for both (measured with a `trap_exit` owner). The seam must therefore report
/// `PortFailure::Eof` for a `0` code — never `Error(0)` — while a non-zero code stays the bare
/// integer.
#[test]
fn driver_failure_of_zero_is_the_eof_route_and_a_nonzero_code_stays_an_integer() {
    let table = driver_api_table();
    let failure_eof = table.driver_failure_eof.expect("populated");
    let failure = table.driver_failure.expect("populated");
    let mut host = LifecycleTestHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    // SAFETY: `port` came from `DriverEnv::as_port` on a live env.
    unsafe {
        assert_eq!(failure_eof(port), 0, "driver_failure_eof accepts a live port");
        assert_eq!(failure(port, 0), 0, "driver_failure(P, 0) accepts a live port");
        assert_eq!(failure(port, 7), 0, "driver_failure(P, 7) accepts a live port");
    }
    assert_eq!(
        host.failures,
        ["Eof".to_string(), "Eof".to_string(), "Error(7)".to_string()],
        "a 0 code is the eof route (reason `normal`, identical to driver_failure_eof); any other \
         code is the bare integer the owner observes"
    );
}

/// `NULL` is **not** the C's invalid-port path (the real VM faults on it, measured), while the
/// sentinel `ERTS_INVALID_ERL_DRV_PORT` (`erl_port.h:60-67`) is the C's own "invalid port" operand,
/// answered with each entry point's failure value. Both must be refused by the one shared
/// `env_for`/`DriverEnv::from_port` guard *without* reaching the host hook — a dereference of either
/// would be UB, so passing here is itself the proof.
#[test]
fn null_and_the_invalid_port_sentinel_are_refused_before_the_host_hook() {
    let table = driver_api_table();
    let failure_eof = table.driver_failure_eof.expect("populated");
    let failure_atom = table.driver_failure_atom.expect("populated");
    let failure_posix = table.driver_failure_posix.expect("populated");
    let failure = table.driver_failure.expect("populated");
    let exit = table.driver_exit.expect("populated");

    let mut host = LifecycleTestHost::new();
    let mut env = DriverEnv::new(&mut host);
    let live = env.as_port();
    let name = c"enoent";

    for (label, operand) in [
        ("NULL", core::ptr::null_mut()),
        ("ERTS_INVALID_ERL_DRV_PORT", ERTS_INVALID_ERL_DRV_PORT),
    ] {
        // SAFETY: neither operand names an environment, which is exactly what `env_for` /
        // `DriverEnv::from_port` document as `None`; `name` is NUL-terminated.
        unsafe {
            assert_eq!(failure_eof(operand), -1, "driver_failure_eof({label})");
            assert_eq!(
                failure_atom(operand, name.as_ptr() as *mut c_char),
                -1,
                "driver_failure_atom({label})"
            );
            assert_eq!(
                failure_posix(operand, libc::ENOENT),
                -1,
                "driver_failure_posix({label})"
            );
            assert_eq!(failure(operand, 0), -1, "driver_failure({label}, 0)");
            assert_eq!(failure(operand, 7), -1, "driver_failure({label}, 7)");
            assert_eq!(exit(operand, 0), -1, "driver_exit({label}, 0)");
            assert_eq!(exit(operand, 3), -1, "driver_exit({label}, 3)");
        }
    }
    assert!(host.failures.is_empty(), "an absent port must never reach the `failure` hook");
    assert!(host.exits.is_empty(), "an absent port must never reach the `port_exit` hook");

    // …and the same entries do reach the hook for the live port, so the refusal above is about the
    // operand, not a dead seam.
    // SAFETY: `live` came from `DriverEnv::as_port` on a live env; `name` is NUL-terminated.
    unsafe {
        assert_eq!(failure_eof(live), 0);
        assert_eq!(failure_atom(live, name.as_ptr() as *mut c_char), 0);
        assert_eq!(exit(live, 0), 0);
    }
    assert_eq!(
        host.failures,
        ["Eof".to_string(), "Atom(\"enoent\")".to_string()],
        "the live port reaches the hook as its own reason"
    );
    assert_eq!(host.exits, [0], "the live port reaches `port_exit` with the raw code");
}
