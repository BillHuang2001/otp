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

//! **Driver registry & port creation** family of the driver host layer (part of the `driver_*` /
//! `erl_drv_*` C ABI, workspace contract **A.4**).
//!
//! This file owns the four slots that act on structure *outside* one port's term services: the
//! emulator's **process-global** driver list and the creation of a brand-new port. Every one is a
//! `#[no_mangle] pub unsafe extern "C" fn` matching one field of the frozen
//! [`ErlDrvApi`](crate::driver::api::ErlDrvApi) table **exactly**, and every one delegates to a
//! [`DriverHost`](crate::driver::host::DriverHost) hook so the runtime stays the authority on what a
//! driver list or a port registry is.
//!
//! # Slots owned by this file
//!
//! | slot | C | carries a port? |
//! |---|---|---|
//! | `add_driver_entry` | `io.c:7640` | no |
//! | `remove_driver_entry` | `io.c:7698` | no |
//! | `driver_lock_driver` | `io.c:7324` | yes |
//! | `driver_create_port` | `io.c:786` | yes (the creator port) |
//!
//! # Where the host comes from
//!
//! The two port-carrying slots recover the port's own [`DriverEnv`] through the shared
//! [`env_for`](super::env_for) helper, exactly as the sibling `port.rs`/`queues.rs`/`lifecycle.rs` do.
//! The two **entry-list** slots have no port operand at all — the C functions act on
//! `driver_list` — so they reach their host through the thread-local publication
//! [`with_current_host`](crate::driver::current_host::with_current_host) that the runtime wraps those
//! entry points in. With no host published they answer the C's failure values **without
//! dereferencing any operand**.
//!
//! # The C semantics reproduced
//!
//! * `add_driver_entry` (`io.c:7640`) is a `void` wrapper over `int erts_add_driver_entry(...)`
//!   (`io.c:7650`) which appends `de` to the global `driver_list` and runs `init_driver` on it; the
//!   C **deliberately discards** its result ("the init is not allowed to fail when drivers are added
//!   by drivers", `io.c:7642-7645`), and so does this slot. [`add_driver_entry_code`] is that
//!   discarded value, kept so the refusal is modelable at all.
//! * `remove_driver_entry` (`io.c:7698`) returns `1` removed, `-1` when the entry is a
//!   **dynamically** loaded driver (`dp->handle != NULL`, "not allowed for dynamic drivers",
//!   `io.c:7709-7713`) and `0` when no such entry is listed.
//! * `driver_lock_driver` (`io.c:7324`) locks the shared object behind **the port's own** driver:
//!   `-1` for an invalid port, `-1` when that driver has no `dlopen` handle
//!   (`(dh = prt->drv_ptr->handle) == NULL`, `io.c:7337` — a statically linked driver), else `0`.
//!   There is no lookup by name in any global table.
//! * `driver_create_port` (`io.c:786-787`) instantiates `name`'s driver and returns the new port; its
//!   failure answer is the invalid-port sentinel [`ERTS_INVALID_ERL_DRV_PORT`] (pointer `-1`,
//!   `erts/emulator/beam/erl_port.h:67`), reproduced here for a null creator port and for a host that
//!   cannot create ports.
//!
//! # Documented deviations from the oracle
//!
//! Each is also stated at the function that carries it; none can produce a *fabricated* success
//! (`CONTEXT.md` §3 — never a wrong result):
//!
//! * The **shape** of the seam, not its answers: the C keeps a real `erts_driver_t`/`Port`, while
//!   this crate keeps no state at all — the host (the runtime) owns the driver list, the loader
//!   handle and the port table. The codes and sentinels are the C's.
//! * `add_driver_entry`/`remove_driver_entry` run with the host that the runtime published around the
//!   entry point. With **no** current host, `add` yields the C failure `-1` (invisible through the
//!   `void` slot) and `remove` reports `0` ("no such entry") — never a panic, never a deref of `de`.
//! * The port operand of `driver_lock_driver`/`driver_create_port` **is** the
//!   [`DriverEnv::as_port`](crate::driver::host::DriverEnv::as_port) pointer, as everywhere else in
//!   part 2; the C's port *id term* has no counterpart in this seam.
//!
//! # `unsafe`
//!
//! The whole file is FFI. Every `unsafe fn` carries a `# Safety` section (its caller contract) and
//! every `unsafe` block a preceding `// SAFETY:` rationale (workspace `CONTEXT.md` §3). No operand is
//! dereferenced — the `ErlDrvEntry*`/`char*` operands are forwarded to the host, which owns their
//! contract — and no failure path panics.

use libc::{c_char, c_int};

use crate::driver::api::ErlDrvApi;
use crate::driver::current_host::with_published_host;
use crate::driver::types::{
    ErlDrvData, ErlDrvEntry, ErlDrvPort, ErlDrvTermData, ERTS_INVALID_ERL_DRV_PORT,
};
use super::env_for;

// ---------------------------------------------------------------------------------------------
// The driver entry list — no port operand
// ---------------------------------------------------------------------------------------------

/// `void add_driver_entry(ErlDrvEntry *de);` — record the statically linked driver entry `de` in the
/// emulator's global driver list (`io.c:7640`).
///
/// The C wrapper **ignores** the result of `erts_add_driver_entry` ("the init is not allowed to fail
/// when drivers are added by drivers", `io.c:7642-7645`), so this slot does too: it hands the request
/// to the current driver host (the runtime publishes it with
/// [`with_current_host`](crate::driver::current_host::with_current_host)) and discards the code
/// [`add_driver_entry_code`] computes. With no host published the request is dropped, exactly as the
/// C's own wrapper leaves nothing an observer could see.
///
/// # Safety
///
/// `de` must be null or point at a live `ErlDrvEntry` that outlives the call (in Erlang, a `static`
/// entry). The pointer is only forwarded, never dereferenced here.
#[no_mangle]
pub unsafe extern "C" fn add_driver_entry(de: *mut ErlDrvEntry) {
    let _ = add_driver_entry_code(de);
}

/// The value behind the `void` [`add_driver_entry`] wrapper: `0` when the host recorded the entry,
/// non-zero when its `init_driver` refused it, and the C's failure value `-1` when **no** current
/// host is published.
///
/// Extracted from the slot so the outcome the C deliberately discards stays assertable
/// (`io.c:7642-7645`).
fn add_driver_entry_code(de: *mut ErlDrvEntry) -> c_int {
    with_published_host(|host| host.add_driver_entry(de)).unwrap_or(-1)
}

/// `int remove_driver_entry(ErlDrvEntry *de);` — remove `de` from the emulator's global driver list
/// (`io.c:7698`).
///
/// Returns `1` when a matching entry was removed, `-1` when it names a **dynamically** loaded driver
/// (`dp->handle != NULL`, "not allowed for dynamic drivers", `io.c:7709-7713`) and `0` when no such
/// entry is listed. This slot adds one case of its own: with no current host it reports `0` ("not
/// found") without touching `de`.
///
/// # Safety
///
/// `de` must be null or point at a live `ErlDrvEntry`. The pointer is only forwarded, never
/// dereferenced here.
#[no_mangle]
pub unsafe extern "C" fn remove_driver_entry(de: *mut ErlDrvEntry) -> c_int {
    with_published_host(|host| host.remove_driver_entry(de)).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Port-carrying slots
// ---------------------------------------------------------------------------------------------

/// `int driver_lock_driver(ErlDrvPort ix);` — lock the driver behind `ix`'s **own** loaded shared
/// object for the rest of the machine's lifetime (`io.c:7324`).
///
/// `-1` for an operand that names no port — the C's invalid-port sentinel
/// `ERTS_INVALID_ERL_DRV_PORT` (its early return) or `NULL` (refused here rather than faulted) — or a
/// host without the hook (the trait default) — which is also the C's answer when the port's driver has
/// no `dlopen` handle (`(dh = prt->drv_ptr->handle) == NULL`, `io.c:7337`, a statically linked
/// driver) — else the host's code (`0` = locked). The C consults the port's own `drv_ptr->handle`,
/// never a global lookup by name.
///
/// # Safety
///
/// `ix` must be null or a live port handle (see [`env_for`](super::env_for)).
#[no_mangle]
pub unsafe extern "C" fn driver_lock_driver(ix: ErlDrvPort) -> c_int {
    // SAFETY: the caller's contract is `env_for`'s contract, restated above.
    match unsafe { env_for(ix) } {
        Some(env) => env.host().lock_driver(ix),
        None => -1,
    }
}

/// `ErlDrvPort driver_create_port(ErlDrvPort creator_port, ErlDrvTermData connected, char *name,
/// ErlDrvData drv_data);` — create a new port owned by the calling driver (`io.c:786-787`): `name`
/// selects the driver to instantiate, `connected` becomes the new port's owner/caller, `drv_data` its
/// driver-private data.
///
/// Returns the new port handle, or the invalid-port sentinel [`ERTS_INVALID_ERL_DRV_PORT`] for a
/// `creator_port` that names no port (`NULL`, or the sentinel itself) or for any host failure — the
/// C's single failure answer (`erl_port.h:67`).
///
/// # Safety
///
/// `creator_port` must be null or a live port handle; `name` must be null or point at a
/// NUL-terminated C string that outlives the call. Neither is dereferenced here.
#[no_mangle]
pub unsafe extern "C" fn driver_create_port(
    creator_port: ErlDrvPort,
    connected: ErlDrvTermData,
    name: *mut c_char,
    drv_data: ErlDrvData,
) -> ErlDrvPort {
    // SAFETY: the caller's contract is `env_for`'s contract, restated above.
    match unsafe { env_for(creator_port) } {
        Some(env) => env.host().create_port(creator_port, connected, name, drv_data),
        None => ERTS_INVALID_ERL_DRV_PORT,
    }
}

// ---------------------------------------------------------------------------------------------
// Table assembly (the `populate` chain step owned by this file)
// ---------------------------------------------------------------------------------------------

/// Populate this family's slots of the frozen [`ErlDrvApi`].
///
/// Called from [`super::DRIVER_API`]'s `populate` chain, so this file — not `mod.rs` — owns the
/// `Some(…)` list for the four slots it defines.
pub(super) const fn populate(mut api: ErlDrvApi) -> ErlDrvApi {
    api.add_driver_entry = Some(add_driver_entry);
    api.remove_driver_entry = Some(remove_driver_entry);
    api.driver_lock_driver = Some(driver_lock_driver);
    api.driver_create_port = Some(driver_create_port);
    api
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use core::ptr::NonNull;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{make_atom, Eterm};

    use crate::driver::host::{with_current_host, DriverEnv, DriverHost};

    /// The smallest possible host — it implements only the four **required** methods, so every
    /// defaulted hook (including this family's four) is exercised as the trait default.
    struct BareHost {
        heap: Heap,
        atoms: AtomTable,
    }

    impl BareHost {
        fn new() -> BareHost {
            BareHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
            }
        }

        /// A dangling port operand: the defaulted hooks must not dereference it.
        fn dangling_port(&self) -> ErlDrvPort {
            NonNull::<crate::driver::types::ErlDrvPort_>::dangling().as_ptr()
        }
    }

    impl DriverHost for BareHost {
        fn heap(&mut self) -> &mut Heap {
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
    }

    /// A host that records every registry/port-creation request and answers with the codes it was
    /// configured with — the runtime's part, played by the test.
    struct RegistryHost {
        heap: Heap,
        atoms: AtomTable,
        /// Every `add_driver_entry` operand, in call order.
        added: Vec<*mut ErlDrvEntry>,
        /// Every `remove_driver_entry` operand.
        removed: Vec<*mut ErlDrvEntry>,
        /// Every `lock_driver` port operand.
        locked: Vec<ErlDrvPort>,
        /// Every `create_port` request: `(creator, connected, name, drv_data)`.
        created: Vec<(ErlDrvPort, ErlDrvTermData, *mut c_char, ErlDrvData)>,
        /// What `add_driver_entry` answers.
        add_code: c_int,
        /// What `remove_driver_entry` answers.
        remove_code: c_int,
        /// What `lock_driver` answers.
        lock_code: c_int,
        /// The handle `create_port` returns.
        port: ErlDrvPort,
    }

    impl RegistryHost {
        fn new() -> RegistryHost {
            RegistryHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                added: Vec::new(),
                removed: Vec::new(),
                locked: Vec::new(),
                created: Vec::new(),
                add_code: 0,
                remove_code: 1,
                lock_code: 0,
                port: 0x1234_isize as ErlDrvPort,
            }
        }
    }

    impl DriverHost for RegistryHost {
        fn heap(&mut self) -> &mut Heap {
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

        fn add_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
            self.added.push(de);
            self.add_code
        }

        fn remove_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
            self.removed.push(de);
            self.remove_code
        }

        fn lock_driver(&mut self, port: ErlDrvPort) -> c_int {
            self.locked.push(port);
            self.lock_code
        }

        fn create_port(
            &mut self,
            creator: ErlDrvPort,
            connected: ErlDrvTermData,
            name: *mut c_char,
            drv_data: ErlDrvData,
        ) -> ErlDrvPort {
            self.created.push((creator, connected, name, drv_data));
            self.port
        }
    }

    /// A dangling (never-dereferenced) entry operand.
    fn dangling_entry() -> *mut ErlDrvEntry {
        NonNull::<ErlDrvEntry>::dangling().as_ptr()
    }

    // -----------------------------------------------------------------------------------------
    // The trait defaults
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_four_registry_hooks_default_to_the_c_failure_values() {
        let mut host = BareHost::new();
        let de = dangling_entry();
        let name = NonNull::<c_char>::dangling().as_ptr();

        // Every default must answer without dereferencing its operand — hence the dangling pointers.
        assert_eq!(host.add_driver_entry(de), -1, "no driver-table service");
        assert_eq!(host.remove_driver_entry(de), 0, "no matching entry");
        assert_eq!(host.lock_driver(host.dangling_port()), -1, "no handle");
        let port = host.create_port(host.dangling_port(), 7, name, core::ptr::null_mut());
        assert_eq!(port, ERTS_INVALID_ERL_DRV_PORT);
        assert_eq!(port as isize, -1, "the C's invalid-port sentinel");
    }

    // -----------------------------------------------------------------------------------------
    // The entry-list slots (no port operand)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn without_a_current_host_the_entry_slots_answer_the_c_values_without_dereferencing() {
        let de = dangling_entry();

        assert_eq!(with_published_host(|host| host.add_driver_entry(de)), None);
        assert_eq!(add_driver_entry_code(de), -1, "the C failure value");
        // SAFETY: both slots only forward `de`; it is never dereferenced.
        unsafe {
            add_driver_entry(de); // the `void` slot: its code is discarded, as in the C
            assert_eq!(remove_driver_entry(de), 0, "`0` = no such entry");
        }
    }

    #[test]
    fn the_entry_slots_forward_to_the_host_published_for_the_scope() {
        let mut outer = RegistryHost::new();
        let mut inner = RegistryHost::new();
        let de = dangling_entry();

        with_current_host(&mut outer, || {
            // SAFETY: the operand is only forwarded to the host.
            unsafe {
                add_driver_entry(de);
                assert_eq!(remove_driver_entry(de), 1, "the host's `removed` answer");
            }

            with_current_host(&mut inner, || {
                // SAFETY: as above.
                unsafe { add_driver_entry(de) };
            });

            // The inner scope is over: the outer host must be current again.
            // SAFETY: as above.
            unsafe { add_driver_entry(de) };
            // And the crate-internal accessor sees exactly the same host.
            assert_eq!(with_published_host(|host| host.add_driver_entry(de)), Some(0));
        });

        assert_eq!(outer.added.len(), 3, "two slot calls plus one direct hook call");
        assert_eq!(outer.removed.len(), 1);
        assert_eq!(inner.added.len(), 1, "only the nested call reached the inner host");
        assert!(inner.removed.is_empty());
        assert_eq!(outer.added[0], de);
    }

    #[test]
    fn an_unwinding_publication_still_restores_the_outer_host() {
        let mut outer = RegistryHost::new();
        let mut inner = RegistryHost::new();
        let de = dangling_entry();

        // Silence the panic this test provokes, then restore the default hook.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        with_current_host(&mut outer, || {
            let unwound = catch_unwind(AssertUnwindSafe(|| {
                with_current_host(&mut inner, || -> () { panic!("boom") });
            }));
            assert!(unwound.is_err(), "the inner publication must unwind");

            // SAFETY: the operand is only forwarded to the host.
            unsafe { add_driver_entry(de) };
        });

        std::panic::set_hook(default_hook);

        assert_eq!(outer.added.len(), 1, "the outer host is current again");
        assert!(inner.added.is_empty());
    }

    // -----------------------------------------------------------------------------------------
    // The port-carrying slots
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_port_carrying_slots_forward_through_the_port_env() {
        let mut host = RegistryHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let name = c"regtest";
        let data: ErlDrvData = 0x55_isize as ErlDrvData;

        // SAFETY: `port` came from `DriverEnv::as_port` on a live env; `name` is NUL-terminated.
        let locked = unsafe { driver_lock_driver(port) };
        // SAFETY: as above.
        let created = unsafe { driver_create_port(port, 42, name.as_ptr() as *mut c_char, data) };

        assert_eq!(locked, 0, "the host locked the port's own driver");
        assert_eq!(created, host.port);
        assert_eq!(host.locked, vec![port]);
        assert_eq!(host.created.len(), 1);
        let (creator, connected, recorded_name, recorded_data) = host.created[0];
        assert_eq!(creator, port);
        assert_eq!(connected, 42);
        assert_eq!(recorded_name, name.as_ptr() as *mut c_char);
        assert_eq!(recorded_data, data);
    }

    #[test]
    fn a_null_creator_port_is_the_c_failure_value_and_is_never_dereferenced() {
        let mut host = RegistryHost::new();
        let mut env = DriverEnv::new(&mut host);
        let null = core::ptr::null_mut();
        let name = c"regtest";

        // SAFETY: a null port is `env_for`'s documented `None`; `name` is NUL-terminated.
        unsafe {
            assert_eq!(driver_lock_driver(null), -1);
            assert_eq!(
                driver_create_port(null, 42, name.as_ptr() as *mut c_char, core::ptr::null_mut()),
                ERTS_INVALID_ERL_DRV_PORT
            );
        }
        assert!(host.locked.is_empty(), "a null port must record nothing");
        assert!(host.created.is_empty());

        // The live env is unaffected: a real port still reaches the host.
        let port = env.as_port();
        // SAFETY: `port` is live.
        assert_eq!(unsafe { driver_lock_driver(port) }, 0);
        assert_eq!(host.locked, vec![port]);
    }

    #[test]
    fn a_host_that_cannot_create_a_port_reports_the_invalid_port_sentinel() {
        // The trait default is the C's failure answer — never a fabricated handle.
        let mut host = BareHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let name = c"regtest";

        // SAFETY: `port` came from `DriverEnv::as_port` on a live env; `name` is NUL-terminated.
        let created =
            unsafe { driver_create_port(port, 1, name.as_ptr() as *mut c_char, core::ptr::null_mut()) };
        assert_eq!(created, ERTS_INVALID_ERL_DRV_PORT);
    }

    // -----------------------------------------------------------------------------------------
    // Table assembly
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_frozen_table_populates_the_registry_family() {
        let table = crate::driver::drv::driver_api_table();
        let populated = [
            table.add_driver_entry.is_some(),
            table.remove_driver_entry.is_some(),
            table.driver_lock_driver.is_some(),
            table.driver_create_port.is_some(),
        ];
        assert!(populated.iter().all(|&p| p), "{populated:?}");
    }
}
