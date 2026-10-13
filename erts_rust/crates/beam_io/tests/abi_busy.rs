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

//! The `set_busy_port` C-ABI host hook (`DriverHost::set_busy_port`) pinned end to end over the
//! real, compiled C driver fixture `tests/fixtures/host_driver.c`.
//!
//! The C's `void set_busy_port(ErlDrvPort port, int on)` (`io.c:5208-5264`) moves the port's busy
//! state and always answers `0` for a live port. `beam_abi` discards that code — the entry callback
//! is `void` and the `driver_*` C function ignores what the hook returns — so the observable is the
//! **flag**: [`AbiHost`] records it per port and [`AbiHost::is_busy`] reads it back (the same
//! "safe companion of a raw hook" shape `DriverState::queue_len` has for `DriverHost::sizeq`).
//!
//! # What is pinned
//!
//! | path | assertion |
//! |---|---|
//! | the raw hook, `AbiHost::set_busy_port` | answers the C's `0`; the flag reads back set, cleared, and idempotently |
//! | the fixture's `busy` start token (`start` runs `set_busy_port(port, 1)`) | the port is busy before it is handed any command |
//! | the fixture's control verbs `25`/`26` (`set_busy_port(port, 1)` / `(port, 0)`) | the flag follows the driver's own requests |
//!
//! # Why the host is built here rather than reached through a live port
//!
//! This crate exposes no accessor for the interior `ErlDrvPort` of an already-opened port (`Port`
//! hands out its `driver_name`, never its driver/environment), so — exactly like
//! `tests/driver_registry_slots.rs` — the test builds an [`AbiHost`] the way
//! `DriverPort::from_library` does (`AbiHost::for_driver` over the loaded fixture's name, `loaded =
//! true`), wraps it in a [`DriverEnv`] and gives the fixture **that** handle. The C `driver_*`
//! functions, the fixture's `start`/`control` callbacks and the `AbiHost` hook are all the real
//! ones; only the host *instance* is the test's, built from identical inputs.
//!
//! # The caller-blocking half is deliberately NOT here (the `beam_rt` handoff)
//!
//! The C's busy state additionally makes `erlang:port_command/2,3` on a busy port answer `false`
//! (`[nosuspend]`), `{error,notsup}` (`[force]`, for a driver without `ERL_DRV_FLAG_SOFT_BUSY`) or
//! **suspend** the caller until the driver un-busies the port. That is the runtime's
//! `port_command` BIF, not this crate's: only the **state** half of the C's busy semantics lives
//! here.
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixture needs a C compiler and the system OTP include directory
//! (`erl_driver.h`); when either is missing the test prints a loud `SKIP:` banner and returns. A
//! load failure is **never** a skip — it means the `driver_*` host symbols are not in this test
//! binary's dynamic symbol table, which is a wiring bug, so the `dlopen` diagnostic is panicked.

mod cdriver;

use std::ffi::c_char;

use beam_abi::driver::host::DriverEnv;
use beam_abi::driver::types::{ErlDrvData, ErlDrvSizeT};

use beam_io::prelude::*;

use cdriver::{fixture_so, force_driver_symbols_link};

/// The raw hook answers the C's `0` and the flag it records is readable through the accessor the
/// port layer consults: set, cleared, and idempotent in both directions.
///
/// This drives `beam_abi`'s `set_busy_port` slot (`driver_api_table().set_busy_port`, the C function
/// a loaded driver resolves out of the loading process) against a live `DriverEnv`, so the whole
/// chain — the frozen API-table slot, the `env_for` host recovery, the `DriverHost` hook and the
/// accessor — is exercised, not just the hook.
#[test]
fn the_set_busy_port_hook_records_and_clears_the_flag_and_answers_zero() {
    let Some(so) = fixture_so() else {
        return;
    };
    force_driver_symbols_link();

    let path = so.to_str().expect("the fixture path is valid UTF-8");
    let name = beam_abi::driver::loader::load(path, "host_driver")
        .expect("the fixture loads")
        .name();
    assert_eq!(name, "host_driver");

    let slot = beam_abi::driver::drv::driver_api_table()
        .set_busy_port
        .expect("set_busy_port is populated in the frozen API table");

    let mut host = AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), name, true)
        .expect("a driver heap");
    assert!(!host.is_busy(), "a fresh port is idle");

    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    // `void set_busy_port(ErlDrvPort, int)` — the callback is `void`, so the hook's `0` is the only
    // code there is (asserted on the host in-memory suite); the observable here is the flag.
    // SAFETY: `port` came from `DriverEnv::as_port` on the live `env` above.
    unsafe { slot(port, 1) };
    assert!(
        host.is_busy(),
        "the hook recorded the busy state on the port's host"
    );

    // Idempotent: a driver may re-assert the state it already asked for.
    // SAFETY: as above.
    unsafe { slot(port, 1) };
    assert!(host.is_busy(), "re-asserting busy keeps the port busy");

    // SAFETY: as above.
    unsafe { slot(port, 0) };
    assert!(!host.is_busy(), "clearing the flag reads back idle");
}

/// The fixture's own `set_busy_port` calls: the `busy` start token (busy **before** any command, the
/// state a `port_command/2,3` on a freshly opened port must consult) and the control verbs `25`/`26`
/// that flip it from a later callback.
///
/// The three fixture paths run through the real compiled `.so`, so this is the end-to-end half of the
/// hook: `driver_mk_port`-style handle identity (the `ErlDrvPort` the driver stashed in `start` is
/// still the one the C `set_busy_port` resolves a host from), the fixture's command parsing and the
/// `AbiHost` flag.
#[test]
fn the_fixture_sets_the_busy_state_at_start_and_through_its_control_verbs() {
    let Some(so) = fixture_so() else {
        return;
    };
    force_driver_symbols_link();

    let path = so.to_str().expect("the fixture path is valid UTF-8");
    let library = beam_abi::driver::loader::load(path, "host_driver").expect("the fixture loads");

    let mut host = AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), "host_driver", true)
        .expect("a driver heap");

    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    // The `busy` start token: `host_start` runs `set_busy_port(port, 1)` itself, so the port is busy
    // **before** it is handed any command.
    let command = c"busy";
    let data: ErlDrvData = library
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture's start callback runs");
    assert!(
        host.is_busy(),
        "the fixture's `busy` start token marked the port busy at start time"
    );

    // Control verb `26` clears it through the handle the fixture stashed at `start`.
    let mut reply: *mut c_char = core::ptr::null_mut();
    let cleared = library
        .control(
            data,
            26,
            core::ptr::null_mut(),
            0,
            &mut reply,
            0 as ErlDrvSizeT,
        )
        .expect("the fixture's control callback runs");
    assert_eq!(cleared, 0, "the C's `set_busy_port` answers no reply");
    assert!(
        !host.is_busy(),
        "the fixture's control verb 26 cleared the busy flag"
    );

    // Control verb `25` sets it again.
    let mut reply: *mut c_char = core::ptr::null_mut();
    let set = library
        .control(
            data,
            25,
            core::ptr::null_mut(),
            0,
            &mut reply,
            0 as ErlDrvSizeT,
        )
        .expect("the fixture's control callback runs");
    assert_eq!(set, 0, "the C's `set_busy_port` answers no reply");
    assert!(
        host.is_busy(),
        "the fixture's control verb 25 set the busy flag"
    );

    // Release the fixture's per-port state (it frees through `driver_alloc`/`driver_free`).
    library.stop(data).expect("the fixture's stop callback runs");
}
