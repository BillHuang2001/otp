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

//! The four driver-registry C-ABI slots, pinned end to end against the real, compiled C driver
//! fixture `tests/fixtures/host_driver.c`.
//!
//! `beam_abi` populates `add_driver_entry`/`remove_driver_entry`/`driver_lock_driver`/
//! `driver_create_port` in the frozen [`ErlDrvApi`] table and delegates them to a
//! [`DriverHost`](beam_abi::driver::host::DriverHost). This crate is the runtime side of that
//! seam ([`beam_io::AbiHost`]): the two no-port slots reach the process-global
//! [`DriverTable`], and the two port-carrying slots consult the **port's own** driver. This test
//! pins the **exact** value each slot produces — including the value it produces with no live port
//! in reach (outside a driver callback) — so a follow-up differential case can assert the same
//! numbers.
//!
//! # What is pinned
//!
//! | slot | operand | value |
//! |---|---|---|
//! | `driver_lock_driver` | a host carrying the fixture's dynamically loaded identity | `0` |
//! | `driver_lock_driver` | a bare host (no identity) or a statically linked one | `-1` |
//! | `driver_lock_driver` | a null / invalid port handle | `-1` |
//! | `remove_driver_entry` | an entry the table has never seen, or a null entry | `0` |
//! | `remove_driver_entry` | a **statically linked** registered entry | `1`, then `0` |
//! | `remove_driver_entry` | a **dynamically loaded** registered entry | `-1` |
//! | `add_driver_entry` then `remove_driver_entry` | one fresh static entry, inside a published-host scope | (recorded), then `1` |
//! | `add_driver_entry` / `remove_driver_entry` | with **no** published host (outside a callback) | dropped, then `0` |
//! | `driver_create_port` | any live port handle, or a null one | `ERTS_INVALID_ERL_DRV_PORT` |
//!
//! The first six rows together are the `0,1,0,-1` sequence
//! `beam_rt/tests/driver.rs::driver_lock_driver_and_the_entry_registry_agree_with_the_oracle` pins
//! against the real system `erl`: that test drives a C driver which, in its `start` callback, calls
//! `driver_lock_driver(port)` (`0`), `add_driver_entry` + `remove_driver_entry` on a helper (`1`),
//! a second `remove_driver_entry` of the same helper (`0`) and `remove_driver_entry` of the port's
//! own, dynamically loaded entry (`-1`) — and asserts the oracle answers the identical
//! `0,1,0,-1`. [`the_four_registry_slots_reproduce_the_oracles_0_1_0_minus_1_sequence`] runs that
//! same sequence directly against this crate's host layer, so the two layers are pinned against
//! the same reference codes.
//!
//! # The one seam this test cannot cross
//!
//! The two port-carrying slots recover the [`DriverEnv`] behind an `ErlDrvPort` and call the hook
//! on **that** host ([`beam_abi::driver::drv`]'s `env_for`). This crate exposes no accessor for the
//! interior `ErlDrvPort` of an already-opened port (`Port` hands out its `driver_name`, never its
//! driver/environment), so the test cannot hand the *live* port's own handle to those slots. It
//! therefore builds a host exactly as [`DriverPort::from_library`] builds one — `AbiHost::for_driver`
//! over the **same** loaded driver identity, `name = "host_driver"`, `loaded = true` — wraps it in a
//! [`DriverEnv`] and calls the slots on that handle. The slot bodies, the `env_for` host recovery and
//! the `AbiHost` hook are all the real ones; only the host *instance* differs, and it is built from
//! the identical inputs, so the values are the ones a live port produces.
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixture needs host facilities a bare container may lack: a C compiler and the
//! system OTP include directory (`erl_driver.h`). When either is missing the test prints a loud
//! `SKIP:` banner and returns. A load failure is **never** a skip: it means the `driver_*` host
//! symbols are not in this test binary's dynamic symbol table (see `tests/abi_driver.rs`), which is
//! a wiring bug, so the `dlopen` diagnostic is panicked with.

mod support;

use std::path::PathBuf;

use beam_abi::driver::host::DriverEnv;
use beam_abi::driver::types::{ErlDrvEntry, ERTS_INVALID_ERL_DRV_PORT};
use beam_io::prelude::*;
use beam_io::DriverPort;

use support::{cc_available, compile_so, erl_include_dir, scratch_dir, skip};

/// The committed C fixture.
fn fixture_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host_driver.c")
}

/// Compile the fixture into a loadable `.so`, or print a loud `SKIP:` and answer `None` when the
/// toolchain is unavailable.
fn fixture_so() -> Option<PathBuf> {
    let Some(include) = erl_include_dir() else {
        skip("driver_registry_slots: no OTP include directory (erl_driver.h) found");
        return None;
    };
    if !cc_available() {
        skip("driver_registry_slots: no C compiler (cc / $CC) available");
        return None;
    }
    let src = fixture_source();
    assert!(
        src.is_file(),
        "the committed fixture is missing: {}",
        src.display()
    );
    let dir = scratch_dir("driver-registry-slots");
    let so = dir.join("host_driver.so");
    assert!(
        compile_so(&src, &so, &include),
        "the host_driver fixture must compile into a shared object at {}",
        so.display()
    );
    Some(so)
}

/// Pull the `#[no_mangle]` `driver_*` host symbols into this test binary's dynamic symbol table
/// (the fixture's `dlopen(RTLD_NOW)` needs them — see `tests/abi_driver.rs`).
fn force_driver_symbols_link() {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
}

/// A statically named `ErlDrvEntry` with no `init` — the shape `add_driver_entry` records.
fn named_entry(name: *const u8) -> ErlDrvEntry {
    ErlDrvEntry {
        driver_name: name as *mut libc::c_char,
        ..ErlDrvEntry::default()
    }
}

/// The `Data` payloads a port-task round reported for `id`.
fn data_of(outcome: &PortTaskOutcome, id: beam_process::PortId) -> Vec<Vec<u8>> {
    outcome
        .notifications
        .iter()
        .filter_map(|notification| match notification {
            PortNotification::Data { port, read } if *port == id => Some(read.flattened()),
            _ => None,
        })
        .collect()
}

/// The four registry slots answer the C's exact codes for a live, dynamically loaded port.
#[test]
fn the_registry_slots_answer_the_cs_codes_for_a_live_loaded_port() {
    let Some(so) = fixture_so() else {
        return;
    };
    force_driver_symbols_link();
    let path = so.to_str().expect("the fixture path is valid UTF-8");

    let api = beam_abi::driver::drv::driver_api_table();
    let add = api.add_driver_entry.expect("add_driver_entry is populated");
    let remove = api
        .remove_driver_entry
        .expect("remove_driver_entry is populated");
    let lock = api
        .driver_lock_driver
        .expect("driver_lock_driver is populated");
    let create = api
        .driver_create_port
        .expect("driver_create_port is populated");

    // The fixture's own name — the identity every port built from it carries.
    let name = beam_abi::driver::loader::load(path, "host_driver")
        .expect("the fixture loads")
        .name();
    assert_eq!(name, "host_driver");

    // ---- A port opened through the normal runtime path ---------------------------------------
    // The same `DriverPort` construction `beam_rt` uses: load the `.so`, wrap it, open it.
    let mut poller = Poller::new().expect("a poll set");
    let mut ports = PortRegistry::new();
    let driver = DriverPort::load(path, "host_driver", "", Box::new(LocalAtomPortHost::new()))
        .expect("the fixture loads and is wrapped in a DriverPort");
    let id = ports
        .open(&mut poller, Box::new(driver))
        .expect("the loaded driver opens as a real port");

    // The port is genuinely live: its `output` callback echoes through `driver_output`.
    ports
        .output(&mut poller, id, b"registry-slots")
        .expect("the driver's output callback runs");
    let outcome = ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the port-task round runs");
    assert_eq!(
        data_of(&outcome, id),
        vec![b"registry-slots".to_vec()],
        "the live port moves data through its driver"
    );

    // ---- The two port-carrying slots ---------------------------------------------------------
    // Built exactly as `DriverPort::from_library` builds the live port's host (same identity:
    // the fixture's name, dynamically loaded). See the module docs for why the live port's own
    // interior `ErlDrvPort` is not reachable here.
    let mut host = AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), name.clone(), true)
        .expect("a driver heap");
    {
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // `driver_lock_driver`: the C consults the port's own driver, which here is a `dlopen`ed
        // object (`loaded = true`), so the answer is `0` — `-1` is reserved for a statically
        // linked driver.
        // SAFETY: `port` came from `DriverEnv::as_port` on the live `env` above.
        assert_eq!(unsafe { lock(port) }, 0, "a loaded port's driver locks");

        // `driver_create_port`: the C's single failure answer — never a fabricated port.
        let create_name = c"host_driver";
        // SAFETY: `port` is live; `create_name` is NUL-terminated.
        assert_eq!(
            unsafe {
                create(
                    port,
                    1,
                    create_name.as_ptr() as *mut libc::c_char,
                    core::ptr::null_mut(),
                )
            },
            ERTS_INVALID_ERL_DRV_PORT,
            "port creation answers the invalid-port sentinel"
        );
    }

    // ---- The two no-port slots (through the thread-local publication) -------------------------
    const PROBE_NAME: &[u8] = b"driver_registry_slots_probe\0";
    let mut entry = named_entry(PROBE_NAME.as_ptr());

    // `remove_driver_entry` of an entry the table has never seen is the C's `0`. It runs outside
    // any publication, so it also proves the "no current host" answer is `0`, not a panic.
    // SAFETY: `entry` is a live `ErlDrvEntry` for this test's body; the pointer is only forwarded.
    assert_eq!(
        unsafe { remove(&mut entry) },
        0,
        "an unregistered entry is not found"
    );

    // `add_driver_entry` with **no** host published is dropped (the C's own `void` wrapper discards
    // the underlying `-1`, so "nothing was recorded" is the whole observable).
    // SAFETY: `entry` is live; the pointer is only forwarded, and there is no host.
    unsafe { add(&mut entry) };
    assert!(
        DriverTable::global()
            .lookup("driver_registry_slots_probe")
            .is_none(),
        "with no published host nothing is recorded"
    );

    // `add_driver_entry` then `remove_driver_entry` on the same fresh static entry. Both run
    // inside a published-host scope, which is the only way the no-port slots reach a host.
    beam_abi::with_current_host(&mut host, || {
        // SAFETY: `entry` is live; the published host records its address.
        unsafe { add(&mut entry) };
        assert_eq!(
            DriverTable::global()
                .lookup("driver_registry_slots_probe")
                .map(|driver| driver.entry),
            Some(&mut entry as *mut ErlDrvEntry),
            "the published host recorded the entry in the process-global table"
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { remove(&mut entry) },
            1,
            "the published host removes the entry"
        );
    });
    assert!(
        DriverTable::global()
            .lookup("driver_registry_slots_probe")
            .is_none(),
        "the entry was removed again"
    );

    ports.close(&mut poller, id).expect("the port closes");
}

/// The four slots reproduce, one for one, the `0,1,0,-1` sequence the oracle pins
/// (`beam_rt/tests/driver.rs::driver_lock_driver_and_the_entry_registry_agree_with_the_oracle`).
///
/// | step | call | code |
/// |---|---|---|
/// | 1 | `driver_lock_driver(port)` — the port's own driver was `dlopen`ed | `0` |
/// | 2 | `add_driver_entry(helper)` then `remove_driver_entry(helper)` | `1` |
/// | 3 | `remove_driver_entry(helper)` again — no such entry | `0` |
/// | 4 | `remove_driver_entry` of the port's **own** loaded entry | `-1` |
///
/// Step 4 is the one that needs the load path's registration: the C records a port's own entry with
/// `erts_add_driver_entry(dp, dh, …)` (`dp->handle != NULL`) — here
/// [`DriverTable::add_loaded`] — which is exactly what turns the misleading `0` ("no such entry")
/// into the C's `-1` ("not allowed for dynamic drivers").
#[test]
fn the_four_registry_slots_reproduce_the_oracles_0_1_0_minus_1_sequence() {
    let Some(so) = fixture_so() else {
        return;
    };
    force_driver_symbols_link();
    let path = so.to_str().expect("the fixture path is valid UTF-8");

    let api = beam_abi::driver::drv::driver_api_table();
    let add = api.add_driver_entry.expect("add_driver_entry is populated");
    let remove = api
        .remove_driver_entry
        .expect("remove_driver_entry is populated");
    let lock = api
        .driver_lock_driver
        .expect("driver_lock_driver is populated");

    // The live port's identity: the fixture, **dynamically loaded** — what
    // `DriverPort::from_library` records (`loaded = true`), so `driver_lock_driver` answers `0`.
    let name = beam_abi::driver::loader::load(path, "host_driver")
        .expect("the fixture loads")
        .name();

    // The load path's registration of the fixture's **own** entry: the C's
    // `erts_add_driver_entry(dp, dh, …)` with `dp->handle != NULL`. `beam_io` has no load path of
    // its own, so [`DriverTable::add_loaded`] is that registration (and the one `create_port`
    // resolves a driver through).
    let library = beam_abi::driver::loader::load(path, "host_driver").expect("the fixture loads");
    let own_entry = library.entry() as *const ErlDrvEntry as *mut ErlDrvEntry;
    DriverTable::global()
        .add_loaded(library)
        .expect("the dynamically loaded driver registers");
    assert_eq!(
        DriverTable::global()
            .lookup("host_driver")
            .map(|driver| driver.dynamic),
        Some(true),
        "the fixture's own entry is listed as a dynamically loaded driver"
    );

    // The oracle's helper: a statically linked entry a driver registers and removes again.
    const HELPER: &[u8] = b"registry_slots_helper\0";
    let mut helper = named_entry(HELPER.as_ptr());

    let mut host = AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), name.clone(), true)
        .expect("a driver heap");
    {
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        // Step 1: `driver_lock_driver` = 0 — the port's own driver was `dlopen`ed.
        // SAFETY: `port` came from `DriverEnv::as_port` on the live `env` above.
        assert_eq!(
            unsafe { lock(port) },
            0,
            "step 1: the port's dynamically loaded driver locks"
        );
    }

    beam_abi::with_current_host(&mut host, || {
        // Step 2: `add_driver_entry` (a statically linked helper) — recorded (the C's underlying
        // `0`; the `void` slot itself discards it, so the table is the observable) ...
        // SAFETY: `helper` is a live `ErlDrvEntry` for this test's whole body.
        unsafe { add(&mut helper) };
        assert_eq!(
            DriverTable::global()
                .lookup("registry_slots_helper")
                .map(|driver| driver.entry),
            Some(&mut helper as *mut ErlDrvEntry),
            "step 2: the published host recorded the helper in the process-global table"
        );
        // ... then `remove_driver_entry` = 1: found and removed.
        // SAFETY: as above.
        assert_eq!(
            unsafe { remove(&mut helper) },
            1,
            "step 2: the registered helper is removed"
        );

        // Step 3: a second `remove_driver_entry` of the same helper = 0 — no such entry.
        // SAFETY: as above.
        assert_eq!(
            unsafe { remove(&mut helper) },
            0,
            "step 3: the helper is already gone"
        );

        // Step 4: `remove_driver_entry` of the port's **own** loaded entry = -1 — the C refuses it.
        // SAFETY: `own_entry` points at the fixture's `ErlDrvEntry`; it is only compared by address.
        assert_eq!(
            unsafe { remove(own_entry) },
            -1,
            "step 4: a dynamically loaded driver entry may not be removed"
        );
        // The `-1` is a refusal, not a removal: the registration is still listed.
        assert_eq!(
            DriverTable::global()
                .lookup("host_driver")
                .map(|driver| driver.dynamic),
            Some(true),
            "step 4: the dynamic registration stays listed after the refused removal"
        );
    });
}

/// The two **port-carrying** slots answer the C's own failure values when no live port (or no
/// per-port driver identity) is behind the handle — the values they produce outside a driver
/// callback.
///
/// `driver_lock_driver` is `-1` for a null/invalid port (the C's `ERTS_INVALID_ERL_DRV_PORT` early
/// return, `io.c:7324`) and for a port whose driver has no `dlopen` handle (`io.c:7337` — a
/// statically linked driver, which a bare [`AbiHost`] models). `driver_create_port` is the
/// invalid-port sentinel for *every* request it cannot honour, `io.c:787`.
#[test]
fn the_port_carrying_slots_answer_their_defaults_without_a_port_or_a_loaded_driver() {
    let api = beam_abi::driver::drv::driver_api_table();
    let lock = api
        .driver_lock_driver
        .expect("driver_lock_driver is populated");
    let create = api
        .driver_create_port
        .expect("driver_create_port is populated");

    // A null (invalid) port: no host is reachable, so the C's own early returns stand. Neither
    // operand is dereferenced — a null port must never be a crash.
    // SAFETY: a null port is the documented "no port" operand.
    assert_eq!(
        unsafe { lock(core::ptr::null_mut()) },
        -1,
        "an invalid port cannot lock its driver"
    );
    // SAFETY: a null creator port is the documented "no port" operand; the name is not dereferenced.
    assert_eq!(
        unsafe { create(core::ptr::null_mut(), 1, core::ptr::null_mut(), core::ptr::null_mut()) },
        ERTS_INVALID_ERL_DRV_PORT,
        "an invalid creator port answers the invalid-port sentinel"
    );

    // A **bare** host (no registered driver identity) models a port whose driver has no `dlopen`
    // handle — the C's statically linked case.
    let mut bare = AbiHost::new(Box::new(LocalAtomPortHost::new())).expect("a driver heap");
    {
        let mut env = DriverEnv::new(&mut bare);
        let port = env.as_port();
        // SAFETY: `port` came from `DriverEnv::as_port` on the live `env` above.
        assert_eq!(
            unsafe { lock(port) },
            -1,
            "a driver with no handle cannot lock"
        );
        let name = c"host_driver";
        // SAFETY: `port` is live; `name` is NUL-terminated.
        assert_eq!(
            unsafe { create(port, 1, name.as_ptr() as *mut libc::c_char, core::ptr::null_mut()) },
            ERTS_INVALID_ERL_DRV_PORT,
            "a host with no port-creation service answers the sentinel"
        );
    }

    // A host carrying a **statically linked** identity is the same `-1`: the C consults the port's
    // own handle, never a lookup by name in any global table.
    let mut statically =
        AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), "host_driver", false)
            .expect("a driver heap");
    {
        let mut env = DriverEnv::new(&mut statically);
        let port = env.as_port();
        // SAFETY: as above.
        assert_eq!(
            unsafe { lock(port) },
            -1,
            "a statically linked identity answers -1"
        );
    }
}
