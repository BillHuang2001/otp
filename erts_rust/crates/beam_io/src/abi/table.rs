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

//! The **process-global driver registry** and its port-creation service — the `beam_io` half of
//! the four `driver_*` slots `beam_abi` reports as `None`
//! (`add_driver_entry`/`remove_driver_entry`/`driver_lock_driver`/`driver_create_port`).
//!
//! # Why this lives here and not in `beam_abi`
//!
//! The four functions operate on **process-global** structure — the C's `driver_list` — while
//! `beam_abi`'s [`DriverHost`](beam_abi::driver::host::DriverHost) seam is *per port* and
//! `beam_abi` is a declaration/ABI crate that owns neither a poller nor a port registry
//! (`erts/emulator/beam/io.c:7640`/`7698`/`7324`/`789`). The table and the port-creation
//! service therefore live here, next to [`DriverPort`](crate::abi::DriverPort) and
//! [`PortRegistry`](crate::PortRegistry) — the two things they need.
//!
//! `beam_abi`'s four slots are populated by forwarding to this type; the exact wiring is
//! *not* in this crate (see the module-internal note in `src/abi/mod.rs` and the crate
//! `CONTEXT.md`).
//!
//! # What each function models
//!
//! | C (`io.c`) | here |
//! |---|---|
//! | `add_driver_entry(de)` | [`DriverTable::add_entry`] — records a **statically linked** entry and runs its `init` (the C's `init_driver`); a non-zero result refuses the entry |
//! | the load path's `erts_add_driver_entry(dp, dh, …)` | [`DriverTable::add_loaded`] — records a **dynamically loaded** driver with its `dlopen` handle (`init` already ran at load) |
//! | `remove_driver_entry(de)` | [`DriverTable::remove_entry`] — `-1` for a dynamic driver (*"Not allowed for dynamic drivers"*), `1` removed, `0` not found |
//! | `driver_lock_driver(ix)` | [`DriverTable::lock_driver`] — `0` when the driver has a handle, `-1` when it is statically linked |
//! | `driver_create_port(creator, pid, name, drv_data)` | [`DriverTable::create_port`] — resolves the registered driver by name and opens a real port from it |
//!
//! # The process-global instance
//!
//! [`DriverTable::global`] is the C's `driver_list` (one per process); [`DriverTable::new`]
//! exists so a test — or a runtime that wants an isolated table — can use its own.
//!
//! # `driver_create_port`'s documented deviation
//!
//! The C re-uses the **creating port's** driver instance and stores the caller's `drv_data`
//! verbatim (`io.c:789-880`: `port->drv_data = drv_data`). This crate has no shared driver
//! instance across ports, so [`DriverTable::create_port`] instead creates a **fresh**
//! instance by running the registered driver's `start` callback (through
//! [`PortRegistry::open`](crate::PortRegistry::open)) — which is also what gives that
//! instance its `ErlDrvPort` handle. A `drv_data` operand is therefore not accepted: the
//! value is `start`'s return. Every other observable (the port exists, is owned by
//! `beam_io`, and runs the driver's callbacks) matches.
//!
//! # `unsafe`
//!
//! This is an explicitly-marked `unsafe` module (workspace `../CONTEXT.md` §3): it stores and
//! dereferences raw `*mut ErlDrvEntry` from a driver shared object and calls the entry's
//! `init` callback. The one `unsafe impl Send` carries a `// SAFETY:` rationale, as does every
//! `unsafe` block.

use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use libc::{c_char, c_int};

use beam_abi::driver::loader::DriverLibrary;
use beam_abi::driver::types::ErlDrvEntry;
use beam_common::Result;
use beam_process::PortId;

use crate::abi::host::PortHost;
use crate::abi::port::DriverPort;
use crate::error;
use crate::poller::Poller;
use crate::port::PortRegistry;

/// One node of the process-global driver list (the C's `erts_driver_t`).
struct Registered {
    /// The driver's `ErlDrvEntry` — static data inside the driver's shared object for a
    /// dynamic driver, or the registrant's own static entry for a statically linked one.
    entry: *mut ErlDrvEntry,
    /// The name the entry declares (`dp->name`); the lookup key.
    name: String,
    /// The loaded object owning the entry, or `None` for a statically linked driver (the
    /// C's `dp->handle == NULL`).
    library: Option<Arc<DriverLibrary>>,
    /// Whether `driver_lock_driver` locked the driver in the machine forever.
    locked: bool,
}

/// The mutable half of a [`DriverTable`] (behind the mutex).
struct Inner {
    /// The registrations, oldest first (the C inserts new nodes at the **head**, so the
    /// newest registration for a name wins — see [`DriverTable::lookup`]).
    drivers: Vec<Registered>,
}

// SAFETY: `Inner` holds raw `ErlDrvEntry` pointers and `Arc<DriverLibrary>`s, neither of
// which is `Send` on its own. Every access is serialized by the `Mutex` in `DriverTable`, so
// there is no data race on the table itself, and the pointed-to state is only *used* on the
// thread that owns the runtime's ports: `create_port` hands an `Arc<DriverLibrary>` clone to a
// `DriverPort`, which is created (and stays) on that one thread — `PortRegistry`/`Poller` are
// themselves `!Send`. The C's `driver_list` is likewise a process-global structure protected
// by `erts_driver_list_lock`.
unsafe impl Send for Inner {}

/// The process-global registry of driver entries (the C's `driver_list`).
pub struct DriverTable {
    /// The registrations. A `Mutex` so the global instance can be touched from any thread
    /// (cargo runs tests in parallel); the runtime itself is single-threaded.
    inner: Mutex<Inner>,
}

/// A read-only snapshot of one registered driver (the C's `erts_driver_t` as seen from the
/// outside — no interior pointers escape except the entry the caller registered).
#[derive(Clone, Debug)]
pub struct RegisteredDriver {
    /// The name the entry declares (`dp->name`).
    pub name: String,
    /// The registered `ErlDrvEntry`.
    pub entry: *mut ErlDrvEntry,
    /// `true` when the entry belongs to a **dynamically loaded** object (the C's
    /// `dp->handle != NULL`): such a driver may not be removed by `remove_driver_entry`.
    pub dynamic: bool,
    /// Whether `driver_lock_driver` locked it in the machine.
    pub locked: bool,
}

impl DriverTable {
    /// An empty table.
    pub fn new() -> DriverTable {
        DriverTable {
            inner: Mutex::new(Inner {
                drivers: Vec::new(),
            }),
        }
    }

    /// The **process-global** driver table (the C's `driver_list`).
    ///
    /// The instance is created on first use and lives for the process's whole life, exactly
    /// as the C's list does — a registered driver is never unloaded by dropping the table.
    pub fn global() -> &'static DriverTable {
        static GLOBAL: OnceLock<DriverTable> = OnceLock::new();
        GLOBAL.get_or_init(DriverTable::new)
    }

    /// Locks the table, recovering from poisoning.
    ///
    /// A panic while the lock is held cannot leave the table in a state that makes a later
    /// access *unsound* (the list is a `Vec` and the operations are single-step), so the
    /// guard is recovered instead of panicking the caller — the workspace's "never panic on
    /// runtime data" rule (`../CONTEXT.md` §3).
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Appends `registered` to the list.
    fn record(&self, registered: Registered) {
        self.lock().drivers.push(registered);
    }

    /// The C's `add_driver_entry(de)`: record a **statically linked** driver entry.
    ///
    /// The entry's `init` callback (`int init(void)`) runs first — the C's `init_driver`
    /// (`io.c:7610-7620`); a non-zero result refuses the registration and the entry is *not*
    /// recorded (the C's `erts_add_driver_entry` removes the node again, `io.c:7668-7675`).
    ///
    /// A **dynamically loaded** driver must be registered with [`DriverTable::add_loaded`]
    /// instead: its `init` has already run at load time, and running it twice would be a
    /// deviation.
    ///
    /// # Errors
    ///
    /// * [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `entry` is null or its
    ///   `driver_name` is null (the C would fault reading the name).
    /// * [`BeamError::Raise`](beam_common::BeamError::Raise) with the `driver_init_failed`
    ///   atom when the entry's `init` callback returns non-zero.
    ///
    /// # Safety
    ///
    /// `entry` must be null or point at a **live** `ErlDrvEntry` that stays valid (its code
    /// mapped and its `driver_name` readable) for at least as long as it is registered — the
    /// same contract `driver_init` gives the C.
    pub unsafe fn add_entry(&self, entry: *mut ErlDrvEntry) -> Result<()> {
        // SAFETY: the caller guarantees `entry` is null or live; the helper null-checks.
        let (name, init) = unsafe { entry_identity(entry) }?;
        if let Some(init) = init {
            // SAFETY: `init` is the entry's own `int(void)` callback, invoked while its code
            // is mapped — the caller's contract, identical to the C's `(*de->init)()`.
            let res = unsafe { init() };
            if res != 0 {
                return Err(error::driver_init_failed());
            }
        }
        self.record(Registered {
            entry,
            name,
            library: None,
            locked: false,
        });
        Ok(())
    }

    /// The load path's `erts_add_driver_entry(dp, dh, …)`: record a **dynamically loaded**
    /// driver's entry together with the object that owns it.
    ///
    /// The entry's `init` is **not** run: `beam_abi::driver::loader::load` already ran it
    /// (`do_load_driver_entry` → `erts_add_driver_entry` → `init_driver`,
    /// `erl_bif_ddll.c:1519`), so running it again here would call the callback twice.
    ///
    /// # Errors
    ///
    /// Whatever the [`DriverLibrary`] reports for a missing name cannot happen (the name was
    /// validated at load); this returns `Ok` for every valid library.
    pub fn add_loaded(&self, library: DriverLibrary) -> Result<()> {
        let name = library.name();
        let entry = library.entry() as *const ErlDrvEntry as *mut ErlDrvEntry;
        self.record(Registered {
            entry,
            name,
            library: Some(Arc::new(library)),
            locked: false,
        });
        Ok(())
    }

    /// The C's `remove_driver_entry(de)`.
    ///
    /// Answers the C's three values: `-1` for a **dynamically loaded** driver (*"Not allowed
    /// for dynamic drivers"*, `io.c:7723`), `1` when the entry was found and removed, `0`
    /// when it was not registered. The newest matching registration is removed first (the C
    /// scans its head-inserted list from the head).
    ///
    /// `entry` is only ever compared by address, never dereferenced, so every value
    /// (including null) is a safe operand.
    pub fn remove_entry(&self, entry: *mut ErlDrvEntry) -> c_int {
        let mut inner = self.lock();
        let Some(index) = inner.drivers.iter().rposition(|node| node.entry == entry) else {
            return 0;
        };
        if inner.drivers[index].library.is_some() {
            return -1;
        }
        inner.drivers.remove(index);
        1
    }

    /// The C's `driver_lock_driver(ix)`: lock the driver in the machine forever.
    ///
    /// Answers `0` after locking, and `-1` when the driver has **no handle** — the C's
    /// statically linked case (`io.c:7335-7338`) — including an unknown name (the C's invalid
    /// port).
    pub fn lock_driver(&self, name: &str) -> c_int {
        let mut inner = self.lock();
        match inner.drivers.iter_mut().rfind(|node| node.name == name) {
            Some(node) if node.library.is_some() => {
                node.locked = true;
                0
            }
            Some(_) | None => -1,
        }
    }

    /// The registration recorded for `name`, or `None`. The **newest** one wins (the C's
    /// `driver_list` is head-inserted, `io.c:7657-7660`).
    pub fn lookup(&self, name: &str) -> Option<RegisteredDriver> {
        let inner = self.lock();
        inner
            .drivers
            .iter()
            .rfind(|node| node.name == name)
            .map(|node| RegisteredDriver {
                name: node.name.clone(),
                entry: node.entry,
                dynamic: node.library.is_some(),
                locked: node.locked,
            })
    }

    /// Every registered driver name, oldest registration first.
    pub fn names(&self) -> Vec<String> {
        let inner = self.lock();
        inner.drivers.iter().map(|node| node.name.clone()).collect()
    }

    /// How many drivers are registered.
    pub fn len(&self) -> usize {
        self.lock().drivers.len()
    }

    /// Whether no driver is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The C's `driver_create_port(creator, pid, name, drv_data)`: open a **real** port from
    /// the registered driver called `name`.
    ///
    /// The port is created through [`PortRegistry::open`](crate::PortRegistry::open), which
    /// runs the driver's `start` callback — the instance's `ErlDrvPort` handle comes from
    /// there (see the module docs for how this differs from the C's `drv_data` operand).
    ///
    /// # Errors
    ///
    /// * [`BeamError::BadArg`](beam_common::BeamError::BadArg) when no driver is registered
    ///   under `name`, or when `command` contains an interior NUL (it cannot be turned into
    ///   the C string `start` takes).
    /// * [`BeamError::Raise`](beam_common::BeamError::Raise) with the `notsup` atom when the
    ///   registration is a **statically linked** entry: Milestone-1 has no
    ///   [`Driver`](crate::driver::Driver) adapter over a raw `ErlDrvEntry` (a loaded driver
    ///   reaches one through [`DriverPort::from_library`]).
    /// * Whatever [`DriverPort::from_library`] or
    ///   [`PortRegistry::open`](crate::PortRegistry::open) returns (a `start` failure, an
    ///   exhausted id space, the term-heap allocation).
    ///
    /// The table lock is **released before** `start` runs, so a driver may re-enter the table
    /// (e.g. call `add_driver_entry`) from its `start` callback without deadlocking.
    pub fn create_port(
        &self,
        ports: &mut PortRegistry,
        poller: &mut Poller,
        name: &str,
        command: &str,
        host: Box<dyn PortHost>,
    ) -> Result<PortId> {
        let library = {
            let inner = self.lock();
            match inner.drivers.iter().rfind(|node| node.name == name) {
                None => return Err(error::badarg()),
                Some(node) => match &node.library {
                    Some(library) => Arc::clone(library),
                    None => return Err(error::notsup()),
                },
            }
        };
        let command = CString::new(command).map_err(|_| error::badarg())?;
        let driver = DriverPort::from_library(library, command, host)?;
        ports.open(poller, Box::new(driver))
    }
}

impl Default for DriverTable {
    fn default() -> DriverTable {
        DriverTable::new()
    }
}

impl std::fmt::Debug for DriverTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("DriverTable")
            .field("len", &inner.drivers.len())
            .finish()
    }
}

/// The name and `init` callback of `entry`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `entry` or its `driver_name` is
/// null (the C's `sys_strcmp`/`init_driver` would fault).
///
/// # Safety
///
/// `entry` must be null or point at a readable `ErlDrvEntry` whose `driver_name` is null or a
/// NUL-terminated C string that stays valid for the duration of the call.
unsafe fn entry_identity(
    entry: *mut ErlDrvEntry,
) -> Result<(String, Option<unsafe extern "C" fn() -> c_int>)> {
    if entry.is_null() {
        return Err(error::badarg());
    }
    // SAFETY: `entry` is non-null and readable (caller contract); `driver_name` is a pointer
    // field of a `#[repr(C)]` struct, so reading it is always valid once the entry is.
    let entry_ref = unsafe { &*entry };
    let name_ptr: *mut c_char = entry_ref.driver_name;
    if name_ptr.is_null() {
        return Err(error::badarg());
    }
    // SAFETY: `driver_name` is a NUL-terminated C string per the driver ABI; the bytes are
    // copied into an owned `String` before the entry is released.
    let name = unsafe { CStr::from_ptr(name_ptr) }
        .to_string_lossy()
        .into_owned();
    Ok((name, entry_ref.init))
}

#[cfg(test)]
mod tests {
    use super::{DriverTable, RegisteredDriver};
    use beam_abi::driver::types::ErlDrvEntry;
    use beam_common::BeamError;
    use libc::c_int;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Each *counted* `init` callback is used by exactly one test and owns its own counter, so the
    // (parallel) tests cannot clobber one another's bookkeeping; a single shared pair was racy.
    static COUNTED_INIT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FAIL_INIT_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// A successful `int init(void)` that counts its calls (owned by one test).
    unsafe extern "C" fn counted_init() -> c_int {
        COUNTED_INIT_CALLS.fetch_add(1, Ordering::SeqCst);
        0
    }

    /// A successful `int init(void)` that counts nothing (the shared default).
    unsafe extern "C" fn noop_init() -> c_int {
        0
    }

    /// An `int init(void)` that always refuses the registration (owned by one test).
    unsafe extern "C" fn failing_init() -> c_int {
        FAIL_INIT_CALLS.fetch_add(1, Ordering::SeqCst);
        3
    }

    /// A statically linked entry named `name` (a `'static` NUL-terminated C string) whose `init`
    /// is `init`.
    fn entry_with(name: *const u8, init: unsafe extern "C" fn() -> c_int) -> ErlDrvEntry {
        ErlDrvEntry {
            init: Some(init),
            driver_name: name as *mut libc::c_char,
            ..ErlDrvEntry::default()
        }
    }

    /// A statically linked entry named `name` with a successful, uncounted `init`.
    fn static_entry(name: *const u8) -> ErlDrvEntry {
        entry_with(name, noop_init)
    }

    const DRIVER_A: &[u8] = b"table_test_a\0";
    const DRIVER_B: &[u8] = b"table_test_b\0";

    #[test]
    fn add_entry_runs_init_and_records_the_name() {
        COUNTED_INIT_CALLS.store(0, Ordering::SeqCst);
        let table = DriverTable::new();
        let mut entry = entry_with(DRIVER_A.as_ptr(), counted_init);

        // SAFETY: `entry` is a live local `ErlDrvEntry` with a `'static` name for the duration
        // of the table's use.
        assert!(unsafe { table.add_entry(&mut entry) }.is_ok());
        assert_eq!(COUNTED_INIT_CALLS.load(Ordering::SeqCst), 1, "init runs once");
        assert_eq!(table.names(), vec!["table_test_a".to_string()]);

        let registered: RegisteredDriver = table.lookup("table_test_a").expect("a registration");
        assert!(!registered.dynamic, "a bare entry is statically linked");
        assert!(!registered.locked);
        assert_eq!(registered.entry, &mut entry as *mut ErlDrvEntry);
        assert!(table.lookup("table_test_missing").is_none());
    }

    #[test]
    fn add_entry_refuses_a_null_entry_or_name() {
        let table = DriverTable::new();
        // SAFETY: a null entry is the documented "nothing to register" operand.
        assert!(matches!(
            unsafe { table.add_entry(core::ptr::null_mut()) },
            Err(BeamError::BadArg)
        ));
        let mut unnamed = ErlDrvEntry::default();
        // SAFETY: `unnamed` is live; its `driver_name` is null.
        assert!(matches!(
            unsafe { table.add_entry(&mut unnamed) },
            Err(BeamError::BadArg)
        ));
        assert!(table.is_empty());
    }

    #[test]
    fn a_non_zero_init_refuses_the_registration() {
        FAIL_INIT_CALLS.store(0, Ordering::SeqCst);
        let table = DriverTable::new();
        let mut entry = entry_with(DRIVER_B.as_ptr(), failing_init);

        // SAFETY: `entry` is live and named.
        let err = unsafe { table.add_entry(&mut entry) }.expect_err("init rejected the entry");
        assert_eq!(err.as_atom(), Some("driver_init_failed"));
        assert_eq!(FAIL_INIT_CALLS.load(Ordering::SeqCst), 1, "init was still called");
        assert!(
            table.is_empty(),
            "a refused entry is not recorded (the C removes it again)"
        );
    }

    #[test]
    fn a_static_driver_cannot_be_locked_and_is_removed_by_pointer() {
        let table = DriverTable::new();
        let mut entry = static_entry(DRIVER_A.as_ptr());
        // SAFETY: `entry` is live and named.
        unsafe { table.add_entry(&mut entry) }.expect("the entry registers");

        assert_eq!(table.lock_driver("table_test_a"), -1, "no handle ⇒ the C's -1");
        assert_eq!(table.lock_driver("table_test_missing"), -1);
        assert_eq!(
            table.lookup("table_test_a").map(|d| d.locked),
            Some(false),
            "a refused lock must not mark the driver locked"
        );

        // SAFETY: the pointer is only compared by address.
        assert_eq!(table.remove_entry(&mut entry), 1, "removed");
        assert_eq!(table.remove_entry(&mut entry), 0, "not found");
        assert_eq!(table.remove_entry(core::ptr::null_mut()), 0);
        assert!(table.is_empty());
    }

    #[test]
    fn the_newest_registration_for_a_name_wins() {
        let table = DriverTable::new();
        let mut first = static_entry(DRIVER_A.as_ptr());
        let mut second = static_entry(DRIVER_A.as_ptr());
        // SAFETY: both locals are live and named for the table's use.
        unsafe {
            table.add_entry(&mut first).expect("the first registers");
            table.add_entry(&mut second).expect("the second registers");
        }
        assert_eq!(table.names().len(), 2);
        let newest = table.lookup("table_test_a").expect("a registration");
        assert_eq!(newest.entry, &mut second as *mut ErlDrvEntry);

        // The C scans its head-inserted list from the head, so the newest match goes first.
        assert_eq!(table.remove_entry(&mut second), 1);
        let remaining = table.lookup("table_test_a").expect("the older one remains");
        assert_eq!(remaining.entry, &mut first as *mut ErlDrvEntry);
    }

    #[test]
    fn create_port_reports_an_unknown_name_and_a_static_entry() {
        let table = DriverTable::new();
        let mut poller = crate::poller::Poller::new().expect("a poll set");
        let mut ports = crate::port::PortRegistry::new();
        let host = || Box::new(super::super::host::LocalAtomPortHost::new()) as Box<dyn super::PortHost>;

        let err = table
            .create_port(&mut ports, &mut poller, "nope", "cmd", host())
            .expect_err("an unknown name is badarg");
        assert_eq!(err.as_atom(), Some("badarg"));

        let mut entry = static_entry(DRIVER_A.as_ptr());
        // SAFETY: `entry` is live and named.
        unsafe { table.add_entry(&mut entry) }.expect("the entry registers");
        let err = table
            .create_port(&mut ports, &mut poller, "table_test_a", "cmd", host())
            .expect_err("a static entry has no driver adapter in M1");
        assert_eq!(
            err.as_atom(),
            Some("notsup"),
            "the M1 gap is a distinguishable typed error"
        );
        assert!(ports.is_empty(), "no port may be created on a failure");
    }

    #[test]
    fn the_global_table_is_one_instance_and_registrations_are_visible_through_it() {
        let first = DriverTable::global();
        let second = DriverTable::global();
        assert!(core::ptr::eq(first, second), "one table per process");

        // A name unique to this test, so a parallel test cannot collide with it.
        const GLOBAL_DRIVER: &[u8] = b"table_test_global\0";
        let mut entry = static_entry(GLOBAL_DRIVER.as_ptr());
        // SAFETY: `entry` is a `'static`-named local that outlives this test (it is removed
        // again below, and the name is static data).
        unsafe { first.add_entry(&mut entry) }.expect("the entry registers");
        assert!(DriverTable::global().lookup("table_test_global").is_some());
        assert_eq!(DriverTable::global().remove_entry(&mut entry), 1);
        assert!(DriverTable::global().lookup("table_test_global").is_none());
    }
}
