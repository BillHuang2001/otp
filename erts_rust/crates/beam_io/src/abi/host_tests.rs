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

//! Path-included tests for [`super`] (`abi::host`) — the driver host seam
//! ([`AbiHost`] over [`beam_abi::driver::host::DriverHost`]) and its port queue.
//!
//! Kept in a sibling file so `host.rs` stays under the workspace's ~1000-line convention;
//! included via `#[path]` as a **child** of `host`, so `super::*` still reaches the module's
//! private items (`DRIVER_HEAP_WORDS`, `AbiHost::heap`) and the crate-private helpers
//! (`interest_from_mode`, `DriverState::take_all_selected`).

use super::{interest_from_mode, AbiHost, LocalAtomPortHost, PortHost};
use beam_abi::driver::host::DriverHost;
use beam_abi::driver::types::{
    ErlDrvEntry, ErlDrvEvent, ErlDrvEvent_, ERTS_INVALID_ERL_DRV_PORT, ERL_DRV_READ,
    ERL_DRV_USE, ERL_DRV_USE_NO_CALLBACK, ERL_DRV_WRITE,
};
use beam_process::PortId;
use beam_term::prelude::{is_atom, make_atom, AtomTable};
use libc::c_int;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::Duration;

use crate::abi::DriverTable;
use crate::driver::PortArms;
use crate::poller::{Interest, Poller};
use crate::queue::ByteQueue;
use crate::DriverContext;

/// A fresh host over a private atom table.
fn new_host() -> AbiHost {
    AbiHost::new(Box::new(LocalAtomPortHost::new())).expect("a driver heap")
}

/// A `driver_select`-style event handle: the fd cast to a pointer (the C convention).
fn event_for(fd: RawFd) -> ErlDrvEvent {
    fd as usize as *mut ErlDrvEvent_
}

/// A fresh poll set plus the two port queues and the arming record one context needs.
fn new_context_parts() -> (Poller, ByteQueue, ByteQueue, PortArms) {
    (
        Poller::new().expect("a poll set"),
        ByteQueue::new(),
        ByteQueue::new(),
        PortArms::default(),
    )
}

#[test]
fn the_local_atom_host_interns_into_its_own_table() {
    let mut table = AtomTable::new();
    let expected = make_atom(table.intern("ok"));
    let mut host = LocalAtomPortHost::with_names(&["ok", "error"]);
    assert_eq!(host.intern_atom("ok"), Some(expected));
    // A repeat is the same atom, and seeding order is insertion order.
    assert_eq!(host.intern_atom("ok"), Some(expected));
    assert_ne!(host.intern_atom("error"), Some(expected));
    assert_eq!(host.connected_pid(), None);
    assert_eq!(host.caller_pid(), None);
    assert!(!host.send_term(None, expected));
    assert_eq!(
        LocalAtomPortHost::default().intern_atom("x").map(is_atom),
        Some(true)
    );
}

#[test]
fn the_new_host_starts_empty_and_has_a_term_heap() {
    let mut host = new_host();
    assert!(host.state().deadline().is_none());
    assert_eq!(host.state().selected_len(), 0);
    assert_eq!(host.state().in_flight(), 0);
    assert_eq!(host.heap().capacity(), super::DRIVER_HEAP_WORDS);
    assert_eq!(host.heap().used(), 0);
}

#[test]
fn driver_output_lands_in_the_port_input_queue() {
    let mut host = new_host();
    let (mut poller, mut input, mut output, mut arms) = new_context_parts();
    let mut ctx = DriverContext::new(
        PortId::new(0),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    );
    host.install(&mut ctx);

    assert_eq!(
        DriverHost::output(&mut host, core::ptr::null_mut(), b"hi"),
        0
    );
    assert_eq!(ctx.input().front(), b"hi");
    assert_eq!(
        DriverHost::output(&mut host, core::ptr::null_mut(), b"!"),
        0
    );
    assert_eq!(ctx.input().front(), b"hi!");
    // Nothing was queued for the device: `driver_output` is owner-bound.
    assert_eq!(ctx.output().len(), 0);
    host.clear();

    // With the context cleared the same call is refused, never a null dereference.
    assert_eq!(
        DriverHost::output(&mut host, core::ptr::null_mut(), b"x"),
        -1
    );
}

#[test]
fn the_host_methods_fail_gracefully_without_an_installed_context() {
    let mut host = new_host();
    assert_eq!(
        DriverHost::select(
            &mut host,
            core::ptr::null_mut(),
            event_for(0),
            ERL_DRV_READ,
            1
        ),
        -1
    );
    assert_eq!(host.state().selected_len(), 0);
}

#[test]
fn select_registers_reregisters_and_deselects_one_fd() {
    let mut host = new_host();
    let (mut poller, mut input, mut output, mut arms) = new_context_parts();
    let (_peer, local) = UnixStream::pair().expect("socketpair(2) succeeds");
    let fd = local.as_raw_fd();
    let event = event_for(fd);

    {
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        );
        host.install(&mut ctx);

        assert_eq!(
            DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_READ, 1),
            0
        );
        assert_eq!(host.state().selected_len(), 1);
        assert_eq!(host.state().fd_armed(true), Some(fd));
        assert_eq!(host.state().fd_armed(false), None);

        // A re-select with a new mode updates the interest instead of adding an entry.
        assert_eq!(
            DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_WRITE, 1),
            0
        );
        assert_eq!(host.state().selected_len(), 1);
        assert_eq!(host.state().fd_armed(true), None);
        assert_eq!(host.state().fd_armed(false), Some(fd));

        // De-selecting releases the poller registration.
        assert_eq!(
            DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_READ, 0),
            0
        );
        assert_eq!(host.state().selected_len(), 0);

        // De-selecting an fd that was never selected still succeeds.
        assert_eq!(
            DriverHost::select(
                &mut host,
                core::ptr::null_mut(),
                event_for(4242),
                ERL_DRV_READ,
                0
            ),
            0
        );
        host.clear();
    }

    assert!(poller.is_empty(), "the poll set holds no registration");
}

/// The two `ERL_DRV_USE` bits are recorded with the registration (never folded into the
/// interest), and a released `ERL_DRV_USE` registration queues its event for the port's
/// `stop_select` (see `DriverState::queued_stop_select`).
#[test]
fn select_records_the_use_release_bits_and_queues_the_release() {
    let mut host = new_host();
    let (mut poller, mut input, mut output, mut arms) = new_context_parts();
    let (_peer, local) = UnixStream::pair().expect("socketpair(2) succeeds");
    let fd = local.as_raw_fd();
    let event = event_for(fd);
    let mut ctx = DriverContext::new(
        PortId::new(0),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    );
    host.install(&mut ctx);

    // `ERL_DRV_READ | ERL_DRV_USE`: the release bit is not an interest, so the registration is
    // still read-only, and nothing is queued while the selection is live.
    assert_eq!(
        DriverHost::select(
            &mut host,
            core::ptr::null_mut(),
            event,
            ERL_DRV_READ | ERL_DRV_USE,
            1
        ),
        0
    );
    assert_eq!(host.state().fd_armed(true), Some(fd));
    assert_eq!(host.state().fd_armed(false), None);
    assert!(host.state_mut().take_queued_stop_select().is_empty());

    // De-selecting queues the event the deselecting call was handed (the C's operand).
    assert_eq!(
        DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_READ, 0),
        0
    );
    assert_eq!(host.state_mut().take_queued_stop_select(), vec![event]);
    assert!(
        host.state_mut().take_queued_stop_select().is_empty(),
        "taking the queue empties it"
    );

    // The release obligation each `mode` records: only `ERL_DRV_USE` without `_NO_CALLBACK`.
    for (mode, owes) in [
        (ERL_DRV_READ, false),
        (ERL_DRV_USE, true),
        (ERL_DRV_READ | ERL_DRV_USE, true),
        (ERL_DRV_WRITE | ERL_DRV_USE, true),
        (ERL_DRV_USE_NO_CALLBACK, false),
        (ERL_DRV_READ | ERL_DRV_USE_NO_CALLBACK, false),
    ] {
        assert_eq!(
            DriverHost::select(&mut host, core::ptr::null_mut(), event, mode, 1),
            0,
            "the selection registers"
        );
        let mut selected = host.state_mut().take_all_selected();
        assert_eq!(selected.len(), 1, "exactly one registration was live ({mode})");
        let (taken_fd, token, taken_owes) = selected.pop().expect("one entry was taken");
        assert_eq!(taken_fd, fd);
        assert_eq!(taken_owes, owes, "the release obligation recorded for mode {mode}");
        ctx.deregister(fd, token)
            .expect("the poller registration is withdrawn");
    }

    host.clear();
    assert!(poller.is_empty(), "every registration was released");
}

#[test]
fn the_mode_bitfield_maps_onto_the_interest_mask() {
    assert_eq!(interest_from_mode(ERL_DRV_READ), Interest::READABLE);
    assert_eq!(interest_from_mode(ERL_DRV_WRITE), Interest::WRITABLE);
    assert_eq!(
        interest_from_mode(ERL_DRV_READ | ERL_DRV_WRITE),
        Interest::BOTH
    );
    // Neither direction set still watches the fd (see the function's docs).
    assert_eq!(interest_from_mode(4), Interest::READABLE);
    assert_eq!(interest_from_mode(0 as c_int), Interest::READABLE);
}

#[test]
fn the_port_timer_arms_reports_and_cancels() {
    let mut host = new_host();

    assert_eq!(
        DriverHost::read_timer(&mut host, core::ptr::null_mut()),
        None,
        "no timer is armed yet"
    );
    assert_eq!(DriverHost::set_timer(&mut host, core::ptr::null_mut(), 50), 0);
    let deadline = host.state().deadline().expect("a deadline was armed");
    assert!(deadline > std::time::Instant::now());
    let remaining = DriverHost::read_timer(&mut host, core::ptr::null_mut())
        .expect("a remaining time is reported");
    assert!(remaining <= 50, "the remaining time never exceeds the request");
    assert!(remaining > 0, "a 50 ms timer has not expired yet: {remaining}");

    assert_eq!(DriverHost::cancel_timer(&mut host, core::ptr::null_mut()), 0);
    assert_eq!(host.state().deadline(), None);
    assert_eq!(DriverHost::read_timer(&mut host, core::ptr::null_mut()), None);

    // An immediate timer reports `Some(0)`, as the C writes `0`.
    assert_eq!(DriverHost::set_timer(&mut host, core::ptr::null_mut(), 0), 0);
    assert_eq!(
        DriverHost::read_timer(&mut host, core::ptr::null_mut()),
        Some(0)
    );
}

static INVOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn count_invocation(_data: *mut libc::c_void) {
    INVOCATIONS.fetch_add(1, Ordering::SeqCst);
}

/// Blocks briefly until `done()` holds, so the async tests are not timing-flaky.
fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if done() {
            return true;
        }
        sleep(Duration::from_millis(5));
    }
    done()
}

#[test]
fn driver_async_runs_the_invoke_and_records_a_completion() {
    INVOCATIONS.store(0, Ordering::SeqCst);
    let mut host = new_host();

    let id = DriverHost::r#async(
        &mut host,
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        Some(count_invocation),
        core::ptr::null_mut(),
        None,
    );
    assert!(id >= 0, "driver_async returns a queue position, got {id}");
    assert!(wait_for(|| INVOCATIONS.load(Ordering::SeqCst) == 1));
    assert_eq!(INVOCATIONS.load(Ordering::SeqCst), 1);
    assert_eq!(host.state().in_flight(), 1);

    // The worker reports its completion; the port thread drains it (what `poll_async` does).
    let mut jobs = 0;
    wait_for(|| {
        jobs = host.state_mut().jobs.drain().len();
        jobs > 0
    });
    assert_eq!(jobs, 1, "exactly one completion is recorded");
    assert_eq!(host.state().in_flight(), 0);
}

#[test]
fn a_null_async_invoke_returns_a_position_and_records_nothing() {
    let mut host = new_host();
    let id = DriverHost::r#async(
        &mut host,
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        None,
        core::ptr::null_mut(),
        None,
    );
    assert!(id >= 0, "the fire-and-forget form still returns a position");
    // Give a (non-existent) worker ample time; nothing must ever be reported.
    sleep(Duration::from_millis(30));
    assert_eq!(host.state_mut().jobs.drain().len(), 0);
    assert_eq!(host.state().in_flight(), 0);
}

#[test]
fn the_port_host_defaults_decline_the_optional_hooks() {
    struct Bare;
    impl PortHost for Bare {
        fn intern_atom(&mut self, _name: &str) -> Option<beam_term::prelude::Eterm> {
            None
        }
    }

    let mut host = AbiHost::new(Box::new(Bare)).expect("a driver heap");
    assert_eq!(host.intern_atom("ok"), None);
    assert_eq!(host.connected_pid(core::ptr::null_mut()), None);
    assert_eq!(host.caller_pid(core::ptr::null_mut()), None);
    assert_eq!(
        host.send_term(core::ptr::null_mut(), None, beam_term::prelude::NIL),
        -1
    );
}

// -------------------------------------------------------------------------------------------
// The process-global driver-registry hooks
// -------------------------------------------------------------------------------------------

/// A statically named `ErlDrvEntry` with no `init` — the shape `add_driver_entry` records.
///
/// A distinct `'static` name per test keeps the shared process-global [`DriverTable`] from
/// letting two parallel tests see each other's registrations (cargo runs them concurrently).
fn named_entry(name: *const u8) -> ErlDrvEntry {
    ErlDrvEntry {
        driver_name: name as *mut libc::c_char,
        ..ErlDrvEntry::default()
    }
}

#[test]
fn add_and_remove_driver_entry_reach_the_process_global_table() {
    const NAME: &[u8] = b"host_test_registry\0";
    let mut host = new_host();
    let mut entry = named_entry(NAME.as_ptr());

    // `remove_driver_entry` of an address the table has never seen is the C's `0`.
    assert_eq!(
        DriverHost::remove_driver_entry(&mut host, &mut entry),
        0,
        "an unregistered entry is the C's 0 (not found)"
    );

    // `add_driver_entry` records it in the process-global table and answers `0`.
    assert_eq!(
        DriverHost::add_driver_entry(&mut host, &mut entry),
        0,
        "recording the entry is the C's void-wrapper success"
    );
    assert_eq!(
        DriverTable::global()
            .lookup("host_test_registry")
            .map(|driver| driver.entry),
        Some(&mut entry as *mut ErlDrvEntry),
        "the entry reached the process-global table"
    );

    // Removing the same pointer answers the C's `1`, and a second removal reports `0`.
    assert_eq!(
        DriverHost::remove_driver_entry(&mut host, &mut entry),
        1,
        "removed"
    );
    assert!(DriverTable::global().lookup("host_test_registry").is_none());
    assert_eq!(
        DriverHost::remove_driver_entry(&mut host, &mut entry),
        0,
        "not found any more"
    );
}

#[test]
fn lock_driver_reports_the_ports_own_driver_handle() {
    // A bare host carries no driver identity: the C's statically-linked case (`handle == NULL`).
    let mut bare = new_host();
    assert_eq!(
        DriverHost::lock_driver(&mut bare, core::ptr::null_mut()),
        -1,
        "a host with no driver identity answers the C's -1"
    );

    // A host carrying a dynamically loaded identity answers `0` even though the name is not
    // registered in the process-global table — the C consults the port's own driver, never
    // `driver_list`.
    let mut loaded =
        AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), "lock_driver_probe", true)
            .expect("a driver heap");
    assert_eq!(
        DriverHost::lock_driver(&mut loaded, core::ptr::null_mut()),
        0,
        "a loaded driver locks regardless of any global registration"
    );

    // An identity that is statically linked is still the C's `-1`.
    let mut statically =
        AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), "lock_driver_probe", false)
            .expect("a driver heap");
    assert_eq!(
        DriverHost::lock_driver(&mut statically, core::ptr::null_mut()),
        -1,
        "a statically linked driver has no handle to lock"
    );
}

#[test]
fn create_port_is_the_invalid_port_sentinel() {
    let mut host = new_host();
    let name = c"host_driver";
    assert_eq!(
        DriverHost::create_port(
            &mut host,
            core::ptr::null_mut(),
            1,
            name.as_ptr() as *mut libc::c_char,
            core::ptr::null_mut(),
        ),
        ERTS_INVALID_ERL_DRV_PORT,
        "a request the host cannot honour answers the C's single failure value"
    );
}

/// The two **no-port** slots reach a host **only** inside a `beam_abi::with_current_host`
/// scope — the thread-local publication the adapter installs around every driver callback.
#[test]
fn the_no_port_slots_reach_the_host_only_inside_a_publication() {
    const NAME: &[u8] = b"host_test_publication\0";
    let table = DriverTable::global();
    let api = beam_abi::driver::drv::driver_api_table();
    let add = api.add_driver_entry.expect("the slot is populated");
    let remove = api.remove_driver_entry.expect("the slot is populated");

    let mut host = new_host();
    let mut entry = named_entry(NAME.as_ptr());

    // Outside any publication there is no current host: the slots answer the C's failure
    // values without touching anything.
    // SAFETY: `entry` is a live `ErlDrvEntry` for this test's whole body; the pointer is only
    // forwarded to a host, and there is none.
    unsafe { add(&mut entry) };
    assert!(
        table.lookup("host_test_publication").is_none(),
        "with no published host nothing is recorded"
    );
    // SAFETY: as above.
    assert_eq!(
        unsafe { remove(&mut entry) },
        0,
        "with no published host the removal is the C's 0"
    );

    // Inside the publication the port's own host handles the request.
    beam_abi::with_current_host(&mut host, || {
        // SAFETY: `entry` is live; the published host records its address.
        unsafe { add(&mut entry) };
        assert!(
            table.lookup("host_test_publication").is_some(),
            "the published host recorded the entry"
        );
    });
    assert!(
        table.lookup("host_test_publication").is_some(),
        "the registration outlives the publication"
    );

    // The publication is gone again, so the entry can only be removed inside one.
    beam_abi::with_current_host(&mut host, || {
        // SAFETY: `entry` is live; the published host removes its address.
        assert_eq!(
            unsafe { remove(&mut entry) },
            1,
            "the published host removes the entry"
        );
    });
    assert!(table.lookup("host_test_publication").is_none());
}

// -----------------------------------------------------------------------------------------------
// The port-queue hooks (`driver_enq` / `driver_pushq` / `driver_deq` / `driver_sizeq` /
// `driver_peekq`), which act on the port's **driver queue** (`DriverState::ioq`) and not on the
// port context's output queue.
// -----------------------------------------------------------------------------------------------

/// The head of `host`'s driver queue as `DriverHost::peekq` views it, or `None` when it is empty.
fn peeked_bytes(host: &mut AbiHost) -> Option<Vec<u8>> {
    let view = DriverHost::peekq(host)?;
    assert_eq!(view.len(), 1, "the queue is one contiguous entry");
    // SAFETY: `iov_base` points into the host's own queue buffer and stays valid until the next
    // `enq`/`pushq`/`deq` (the lifetime the C gives the pointer). The bytes are copied out
    // immediately, before `host` is touched again.
    Some(
        unsafe { core::slice::from_raw_parts(view[0].iov_base as *const u8, view[0].iov_len) }
            .to_vec(),
    )
}

/// The five queue hooks move bytes on the driver's own queue, in the C's order; they need no
/// installed `DriverContext` (the C's `drvport2ioq(ix)` has none either) and never reach the port
/// context's output queue.
#[test]
fn the_queue_hooks_hold_the_drivers_own_queue() {
    let mut host = new_host();
    let (mut poller, mut input, mut output, mut arms) = new_context_parts();
    let mut ctx = DriverContext::new(
        PortId::new(0),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    );
    host.install(&mut ctx);

    // An untouched queue: `driver_sizeq` is `0` and `driver_peekq` reports the C's NULL view.
    assert_eq!(DriverHost::sizeq(&mut host), 0);
    assert_eq!(peeked_bytes(&mut host), None);
    assert_eq!(host.state().queue_len(), 0);

    // `driver_enq` appends (`io.c:6790`), and the three ways of reading the queue agree.
    assert_eq!(DriverHost::enq(&mut host, b"ab", false), 0);
    assert_eq!(DriverHost::enq(&mut host, b"cd", false), 0);
    assert_eq!(DriverHost::sizeq(&mut host), 4);
    assert_eq!(host.state().queue_len(), 4);
    assert_eq!(peeked_bytes(&mut host), Some(b"abcd".to_vec()));

    // `driver_pushq` prepends (`io.c:6800`) ...
    assert_eq!(DriverHost::pushq(&mut host, b"xy", false), 0);
    assert_eq!(peeked_bytes(&mut host), Some(b"xyabcd".to_vec()));
    // ... and an empty one is the C's `len == 0` early return, not a no-op reordering.
    assert_eq!(DriverHost::pushq(&mut host, b"", false), 0);
    assert_eq!(peeked_bytes(&mut host), Some(b"xyabcd".to_vec()));

    // `driver_deq`: a zero drop reports an unchanged queue, a partial one drops from the head.
    assert_eq!(DriverHost::deq(&mut host, 0), 0);
    assert_eq!(host.state().queue_len(), 6);
    assert_eq!(DriverHost::deq(&mut host, 2), 2);
    assert_eq!(host.state().queue_len(), 4);
    assert_eq!(peeked_bytes(&mut host), Some(b"abcd".to_vec()));

    // `driver_output` is the *other* direction: it lands in the port's input queue while the
    // driver queue keeps its bytes.
    assert_eq!(DriverHost::output(&mut host, core::ptr::null_mut(), b"hi"), 0);
    assert_eq!(ctx.input().front(), b"hi");
    assert_eq!(host.state().queue_len(), 4, "the driver queue is untouched");
    assert_eq!(ctx.output().len(), 0, "nothing reached the port's output queue");

    // The documented deviation: an over-long `driver_deq` clamps rather than failing (`-1`), so
    // the queue ends up empty instead of holding bytes the caller believes it dropped.
    assert_eq!(DriverHost::deq(&mut host, 100), 4);
    assert_eq!(DriverHost::sizeq(&mut host), 0);
    assert_eq!(peeked_bytes(&mut host), None);
    // A negative length is impossible through the C API (its operand is an `ErlDrvSizeT`) and is
    // refused rather than silently treated as a drain.
    assert_eq!(DriverHost::deq(&mut host, -1), -1);

    // The queue is the host's own state, so it is reachable with no callback context installed.
    host.clear();
    assert_eq!(DriverHost::enq(&mut host, b"z", false), 0);
    assert_eq!(peeked_bytes(&mut host), Some(b"z".to_vec()));
}

/// A de-select whose `DriverContext::deregister` never runs keeps the fd record **and** its
/// `ERL_DRV_USE` release obligation: withdrawing the record there would lose the driver's
/// `stop_select` for an fd the poller still holds.
#[test]
fn a_deselect_that_cannot_deregister_keeps_the_record_and_the_release() {
    let mut host = new_host();
    let (mut poller, mut input, mut output, mut arms) = new_context_parts();
    let (_peer, local) = UnixStream::pair().expect("socketpair(2) succeeds");
    let fd = local.as_raw_fd();
    let event = event_for(fd);

    {
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        );
        host.install(&mut ctx);
        assert_eq!(
            DriverHost::select(
                &mut host,
                core::ptr::null_mut(),
                event,
                ERL_DRV_READ | ERL_DRV_USE,
                1
            ),
            0
        );
        assert_eq!(host.state().selected_len(), 1);
        host.clear();

        // No context is installed, so the poller registration cannot be withdrawn: the de-select
        // fails and changes nothing.
        assert_eq!(
            DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_READ, 0),
            -1
        );
        assert_eq!(
            host.state().selected_len(),
            1,
            "the registration is kept for a later withdrawal"
        );
        assert!(
            host.state_mut().take_queued_stop_select().is_empty(),
            "nothing was released"
        );

        // The same de-select through a live context withdraws the registration and queues the
        // release, whose operand is the event the deselecting call was handed.
        host.install(&mut ctx);
        assert_eq!(
            DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_READ, 0),
            0
        );
        assert_eq!(host.state().selected_len(), 0);
        assert_eq!(host.state_mut().take_queued_stop_select(), vec![event]);
        host.clear();
    }

    assert!(poller.is_empty(), "the poll set holds no registration");
}
