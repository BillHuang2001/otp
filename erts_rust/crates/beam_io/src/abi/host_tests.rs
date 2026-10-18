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
use beam_common::Result;
use beam_process::PortId;
use beam_term::prelude::{is_atom, make_atom, AtomTable};
use libc::c_int;
use std::collections::VecDeque;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::Duration;

use crate::abi::DriverTable;
use crate::driver::{Driver, PortArms};
use crate::poller::{Interest, Poller};
use crate::queue::ByteQueue;
use crate::port::Port;
use crate::port_task::{PortNotification, ReadMessage};
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
    assert!(!host.is_busy(), "a fresh port is not busy");
    assert!(!host.state().is_busy());
}

/// `set_busy_port` (`io.c:5208`) records the port's busy state and answers the C's `0`, and the
/// recorded state is readable through [`AbiHost::is_busy`] (the accessor the port layer consults).
///
/// The C's `void set_busy_port` only moves the port's scheduler flags, so the host's return code is
/// discarded by `beam_abi`'s `driver_*` function — the observable is the flag. This pins both: the
/// hook's `0` and the flag it sets/clears, in both directions.
#[test]
fn set_busy_port_records_and_clears_the_busy_flag() {
    let mut host = new_host();
    assert!(!host.is_busy(), "the port starts idle");

    assert_eq!(
        DriverHost::set_busy_port(&mut host, true),
        0,
        "a live port accepts the busy transition (the C's 0)"
    );
    assert!(
        host.is_busy(),
        "the busy flag is recorded on the port's host state"
    );
    assert!(host.state().is_busy());

    assert_eq!(DriverHost::set_busy_port(&mut host, false), 0);
    assert!(!host.is_busy(), "a cleared busy flag reads back idle");
    assert!(!host.state().is_busy());

    // Idempotent in both directions (a driver may re-assert the same state).
    assert_eq!(DriverHost::set_busy_port(&mut host, true), 0);
    assert_eq!(DriverHost::set_busy_port(&mut host, true), 0);
    assert!(host.is_busy());
}
/// `driver_output` is the C's `driver_output2(ix, NULL, 0, buf, len)` (`io.c:6405-6415`), so every
/// call is **one read message of its own**: its bytes are the read's *body* (never a header), and
/// nothing is banked on the port's input accumulator — which is precisely what used to coalesce two
/// calls made in one callback into a single `{data, _}`.
///
/// A zero-length call is still a call, so it stages a read with a **present** empty body
/// (`Some(vec![])` — the C's non-NULL `buf` of length 0): the oracle renders it `{data, []}` in
/// `[list]` mode and `{data, <<>>}` in `[binary]` mode, i.e. a real message, unlike the
/// `buf == NULL` header-only shape [`driver_output2`] carries as `body: None`.
#[test]
fn driver_output_stages_one_body_only_read_per_call() {
    let mut host = new_host();
    let (mut poller, mut input, mut output, mut arms) = new_context_parts();
    let mut reads: VecDeque<ReadMessage> = VecDeque::new();
    {
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        )
        .with_reads(&mut reads);
        host.install(&mut ctx);

        assert_eq!(
            DriverHost::output(&mut host, core::ptr::null_mut(), b"hi"),
            0
        );
        assert_eq!(
            DriverHost::output(&mut host, core::ptr::null_mut(), b"!"),
            0
        );
        // `driver_output(p, "", 0)`: accepted, and one message (not the old silent drop).
        assert_eq!(DriverHost::output(&mut host, core::ptr::null_mut(), b""), 0);
        host.clear();

        // Nothing was banked on the accumulator, so no call can merge with another.
        assert!(
            ctx.input().front().is_empty(),
            "`driver_output` never banks bytes: it stages a read of its own"
        );
        // Nothing was queued for the device either: `driver_output` is owner-bound.
        assert_eq!(ctx.output().len(), 0);
    }
    assert_eq!(
        reads,
        VecDeque::from([
            ReadMessage::body_only(b"hi".to_vec()),
            ReadMessage::body_only(b"!".to_vec()),
            // A present, empty body — different from the C's `NULL` body (`body: None`).
            ReadMessage::body_only(Vec::new()),
        ]),
        "one body-only read per call, in call order, never merged"
    );
}

/// `driver_output2(ix, hbuf, hlen, buf, len)` keeps the header/body split, and its `buf == NULL`
/// form is a **different** message from a present-but-empty body (the pinned oracle table on
/// [`ReadMessage`]): `body: None` is the header-only proper list, `body: Some(vec![])` the
/// `[H…|<<>>]` improper one.
#[test]
fn driver_output2_keeps_the_header_apart_and_the_null_body_distinct() {
    let mut host = new_host();
    let (mut poller, mut input, mut output, mut arms) = new_context_parts();
    let mut reads: VecDeque<ReadMessage> = VecDeque::new();
    {
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        )
        .with_reads(&mut reads);
        host.install(&mut ctx);

        // `driver_output2(p, "hdr", 3, NULL, 0)` — no binary tail at all.
        assert_eq!(
            DriverHost::output2(&mut host, core::ptr::null_mut(), b"hdr", None),
            0
        );
        // `driver_output2(p, NULL, 0, NULL, 0)` — the C's `{data, []}` / `{data, []}` row.
        assert_eq!(
            DriverHost::output2(&mut host, core::ptr::null_mut(), b"", None),
            0
        );
        // `driver_output2(p, "hdr", 3, "", 0)` — a present, empty body.
        assert_eq!(
            DriverHost::output2(&mut host, core::ptr::null_mut(), b"hdr", Some(b"")),
            0
        );
        // `driver_output2(p, "hdr", 3, "body", 4)` — the full split.
        assert_eq!(
            DriverHost::output2(&mut host, core::ptr::null_mut(), b"hdr", Some(b"body")),
            0
        );
        host.clear();
    }
    assert_eq!(
        reads,
        VecDeque::from([
            ReadMessage {
                header: b"hdr".to_vec(),
                body: None,
            },
            ReadMessage {
                header: Vec::new(),
                body: None,
            },
            ReadMessage {
                header: b"hdr".to_vec(),
                body: Some(Vec::new()),
            },
            ReadMessage {
                header: b"hdr".to_vec(),
                body: Some(b"body".to_vec()),
            },
        ]),
        "the split survives the host seam, and `None` stays distinguishable from `Some([])`"
    );
}

/// With **no** read sink installed (a bare `DriverContext`, i.e. a driver driven outside a port)
/// both output hooks answer the C's `-1`: there is no owner to report to, and refusing beats
/// flattening the read into the accumulator. A cleared context (a callback that has returned) is
/// the same refusal, never a null dereference.
#[test]
fn driver_output_without_a_read_sink_is_the_c_minus_one() {
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
        DriverHost::output(&mut host, core::ptr::null_mut(), b"x"),
        -1
    );
    assert_eq!(
        DriverHost::output2(&mut host, core::ptr::null_mut(), b"x", None),
        -1
    );
    assert!(ctx.input().front().is_empty(), "nothing was banked instead");
    host.clear();

    assert_eq!(
        DriverHost::output(&mut host, core::ptr::null_mut(), b"x"),
        -1,
        "no installed context is a refusal, never a null dereference"
    );
}

// -----------------------------------------------------------------------------------------------
// The end-to-end owner path: a driver reporting through a real `AbiHost` — exactly how a loaded C
// driver's `driver_output*` calls arrive — down to the `PortNotification`s the runtime renders.
// -----------------------------------------------------------------------------------------------

/// One `driver_output*` call a [`HostOutputDriver`] makes in its single `ready_input`.
enum HostCall {
    /// `driver_output(ix, buf, len)` (`io.c:6405-6415`): one body-only read.
    Output(&'static [u8]),
    /// `driver_output2(ix, hbuf, hlen, buf, len)`, `None` being the C's `buf == NULL`.
    Output2(&'static [u8], Option<&'static [u8]>),
}

/// A driver that hands its reads up through a real [`AbiHost`] rather than through
/// `DriverContext::deliver_read` directly, so the in-crate chain `driver_*` hook → host →
/// `deliver_read` → port read queue → [`PortNotification::Data`] is exercised without a `cc`-built
/// fixture.
struct HostOutputDriver {
    /// The host a loaded C driver's `driver_output*` would be called on.
    host: AbiHost,
    /// What the driver reports, in call order.
    calls: Vec<HostCall>,
    /// One shot, so a second `ready_input` reports nothing.
    fed: bool,
}

impl Driver for HostOutputDriver {
    fn name(&self) -> &str {
        "host-output-test"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
        Ok(0)
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if self.fed {
            return Ok(());
        }
        self.fed = true;
        self.host.install(ctx);
        for call in &self.calls {
            let rc = match call {
                HostCall::Output(bytes) => {
                    DriverHost::output(&mut self.host, core::ptr::null_mut(), bytes)
                }
                HostCall::Output2(header, body) => DriverHost::output2(
                    &mut self.host,
                    core::ptr::null_mut(),
                    header,
                    *body,
                ),
            };
            assert_eq!(rc, 0, "the port accepts every `driver_output*` call");
        }
        self.host.clear();
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// The whole chain reaches the owner: **one** `PortNotification::Data` per `driver_output*` call, in
/// call order, with the header/body split intact and nothing dropped. A zero-length
/// `driver_output(p, "", 0)` is a real message (the oracle's `{data, []}` / `{data, <<>>}`) — the
/// accumulator-gated report this replaced silently lost it, and the coalescing one merged two calls.
#[test]
fn the_host_output_path_reaches_the_port_owner_one_message_per_call() {
    let driver = HostOutputDriver {
        host: new_host(),
        calls: vec![
            HostCall::Output(b"hi"),
            // `driver_output(p, "", 0)`: a present, empty body — still one message.
            HostCall::Output(b""),
            // `driver_output2(p, "hdr", 3, NULL, 0)`: header only, no binary tail.
            HostCall::Output2(b"hdr", None),
            // `driver_output2(p, NULL, 0, "body", 4)`: all body.
            HostCall::Output2(b"", Some(b"body")),
        ],
        fed: false,
    };
    let mut poller = Poller::new().expect("a poll set");
    let mut port = Port::new(PortId::new(1), Box::new(driver));

    port.ready_input(&mut poller).expect("the read succeeds");

    let reported: Vec<(Vec<u8>, Option<Vec<u8>>)> = port
        .take_input_notifications()
        .into_iter()
        .map(|notification| match notification {
            PortNotification::Data { read, .. } => (read.header, read.body),
            other => panic!("the port reported an unexpected notification: {other:?}"),
        })
        .collect();
    assert_eq!(
        reported,
        vec![
            (Vec::new(), Some(b"hi".to_vec())),
            (Vec::new(), Some(Vec::new())),
            (b"hdr".to_vec(), None),
            (Vec::new(), Some(b"body".to_vec())),
        ],
        "one message per call, in call order, the split preserved, nothing dropped or merged"
    );
    assert!(
        port.take_input_notifications().is_empty(),
        "reported exactly once"
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

/// The head of `host`'s driver queue, read through the **safe** [`AbiHost::queue_head`] accessor
/// and cross-checked against the raw [`DriverHost::peekq`] view it mirrors, or `None` when empty.
///
/// Reading the bytes off the raw view needs `unsafe` (`iov_base` is a raw pointer); this helper
/// needs none, which is what pins `queue_head` as a sufficient safe wrapper for `driver_peekq`.
fn peeked_bytes(host: &mut AbiHost) -> Option<Vec<u8>> {
    let view = DriverHost::peekq(host);
    let head = AbiHost::queue_head(host);
    match (&view, head) {
        (None, None) => None,
        (Some(view), Some(head)) => {
            assert_eq!(view.len(), 1, "the queue is one contiguous entry");
            assert_eq!(
                head.len(),
                view[0].iov_len,
                "the safe accessor sees exactly the raw view's head length"
            );
            assert_eq!(
                head.as_ptr(),
                view[0].iov_base as *const u8,
                "the raw view addresses the safe accessor's buffer"
            );
            Some(head.to_vec())
        }
        (view, head) => panic!(
            "driver_peekq and AbiHost::queue_head disagree on emptiness \
             (raw view present: {}, safe head present: {})",
            view.is_some(),
            head.is_some()
        ),
    }
}

/// [`AbiHost::queue_head`] is the safe companion of `driver_peekq`: it hands out the same head
/// bytes as a slice without `unsafe`, is `None` exactly when the raw view is, needs no installed
/// `DriverContext` (the queue is the host's own state, as the C's `drvport2ioq(ix)` is), and is
/// bounded to the queue-mutation lifetime by taking `&self` while every mutating hook takes
/// `&mut self`.
#[test]
fn the_queue_head_accessor_is_the_safe_peekq_view() {
    let mut host = new_host();

    // Nothing queued: the raw hook reports its `NULL` view and the accessor `None`.
    assert!(DriverHost::peekq(&mut host).is_none());
    assert_eq!(host.queue_head(), None);
    assert_eq!(host.state().queue_len(), 0);

    // A queued head: the accessor returns the raw view's bytes, of the raw view's length, and
    // `driver_sizeq` counts the same bytes.
    assert_eq!(DriverHost::enq(&mut host, b"hello", false), 0);
    let view = DriverHost::peekq(&mut host).expect("a non-empty queue has a view");
    assert_eq!(view.len(), 1, "the queue is one contiguous entry");
    let head = host.queue_head().expect("a non-empty queue has a head");
    assert_eq!(head, b"hello");
    assert_eq!(head.len(), view[0].iov_len);
    assert_eq!(
        head.as_ptr(),
        view[0].iov_base as *const u8,
        "the raw view addresses the accessor's buffer"
    );
    assert_eq!(
        host.state().queue_len(),
        head.len(),
        "the `sizeq` companion agrees with the `peekq` companion"
    );

    // The accessor follows the queue: `pushq` prepends and `deq` drops from the head.
    assert_eq!(DriverHost::pushq(&mut host, b"xy", false), 0);
    assert_eq!(host.queue_head(), Some(&b"xyhello"[..]));
    assert_eq!(DriverHost::deq(&mut host, 2), 2);
    assert_eq!(host.queue_head(), Some(&b"hello"[..]));

    // A full drain takes the accessor back to `None`, like the raw view.
    assert_eq!(DriverHost::deq(&mut host, 100), 5);
    assert_eq!(host.queue_head(), None);
    assert!(DriverHost::peekq(&mut host).is_none());
}

/// The five queue hooks move bytes on the driver's own queue, in the C's order; they need no
/// installed `DriverContext` (the C's `drvport2ioq(ix)` has none either) and never reach the port
/// context's output queue.
#[test]
fn the_queue_hooks_hold_the_drivers_own_queue() {
    let mut host = new_host();
    let mut reads: VecDeque<ReadMessage> = VecDeque::new();
    {
        let (mut poller, mut input, mut output, mut arms) = new_context_parts();
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        )
        .with_reads(&mut reads);
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

        // `driver_output` is the *other* direction: it stages a read for the port's owner (see
        // `driver_output_stages_one_body_only_read_per_call`) instead of touching either queue.
        assert_eq!(
            DriverHost::output(&mut host, core::ptr::null_mut(), b"hi"),
            0
        );
        assert!(
            ctx.input().front().is_empty(),
            "nothing is banked on the accumulator"
        );
        assert_eq!(
            ctx.output().len(),
            0,
            "nothing reached the port's output queue"
        );
        assert_eq!(host.state().queue_len(), 4, "the driver queue is untouched");

        // The documented deviation: an over-long `driver_deq` clamps rather than failing (`-1`), so
        // the queue ends up empty instead of holding bytes the caller believes it dropped.
        assert_eq!(DriverHost::deq(&mut host, 100), 4);
        assert_eq!(DriverHost::sizeq(&mut host), 0);
        assert_eq!(peeked_bytes(&mut host), None);
        // A negative length is impossible through the C API (its operand is an `ErlDrvSizeT`) and
        // is refused rather than silently treated as a drain.
        assert_eq!(DriverHost::deq(&mut host, -1), -1);
        host.clear();
    }
    assert_eq!(
        reads,
        VecDeque::from([ReadMessage::body_only(b"hi".to_vec())]),
        "`driver_output` stages one read of its own for the owner"
    );

    // The queue is the host's own state, so it is reachable with no callback context installed.
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
