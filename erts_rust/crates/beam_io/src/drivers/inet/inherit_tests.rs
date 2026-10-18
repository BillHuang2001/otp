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

//! The accepted-socket option inheritance suite — the C's `tcp_inet_copy`
//! (`inet_drv.c:11795`-`:11839`) as implemented by [`SocketOptions::inherited_from`] and
//! [`TcpInetDriver::from_accepted`].
//!
//! Pure unit tests: **no** poller, no [`crate::PortRegistry`], no socket and no `erl`. The mapping
//! is a pure function of the two option stores, so the whole positive half (the fifteen inherited
//! fields) and the whole negative half (the fields the C resets) are decided here — the live
//! `accept(2)` evidence belongs to the integration suites.
//!
//! Every case is built so it **fails without the copy**: the listener's inherited values are all
//! non-default, and the child's non-inherited ones are non-default too (and different from the
//! listener's), so neither half can pass by accident.

use beam_common::BeamError;

use super::opt::SocketOptions;
use super::TcpInetDriver;

/// `{mode, binary}` (`INET_MODE_BINARY`, `inet_drv.c:779`).
const MODE_BINARY: i32 = 1;

/// `{deliver, port}` (`INET_DELIVER_PORT`, `inet_drv.c:783`).
const DELIVER_PORT: i32 = 0;

/// `{active, true}` (`INET_ACTIVE`, `inet_drv.c:787`).
const ACTIVE_TRUE: i32 = 1;

/// `{active, N}` (`INET_MULTI`, `inet_drv.c:789`).
const ACTIVE_MULTI: i32 = 3;

/// The listener's option store: every **inherited** field non-default, plus non-default values for
/// the fields the C does **not** copy (so the negative half has a value to leak if the mapping is
/// wrong).
fn listener_options() -> SocketOptions {
    SocketOptions {
        // --- the inherited set (`tcp_inet_copy`'s copy list, `:11809`-`:11838`) --------------
        packet: 2,          // {packet, 2}   — the C's `inet.htype`
        packet_size: 100,   // {packet_size, 100}
        header: 1,          // {header, 1}
        mode: MODE_BINARY,  // {mode, binary}
        deliver: DELIVER_PORT,
        exit_on_close: 0,   // {exit_on_close, false}
        buffer_size: 4096,  // {buffer, 4096}
        high: 4096,         // {high_watermark, 4096}
        low: 2048,          // {low_watermark, 2048}
        send_timeout: 500,  // {send_timeout, 500}
        send_timeout_close: 1,
        show_econnreset: true,  // TCP_ADDF_SHOW_ECONNRESET
        read_ahead: false,      // TCP_ADDF_NO_READ_AHEAD
        msgq_high: 1000,        // {high_msgq_watermark, 1000}
        msgq_low: 500,          // {low_msgq_watermark, 500}
        // --- the fields the C does NOT copy, deliberately pushed off their defaults ----------
        active: ACTIVE_MULTI,
        active_count: 7,
        debug: 1,
        netns: Some(b"/netns/listener".to_vec()),
        read_packets: 3,
        line_delim: b'|',
        buffer_set: false,
        delay_send: true,   // TCP_ADDF_DELAY_SEND — outside the C's mask
    }
}

/// The child store an accepted socket starts from: defaults with **its own** non-default values for
/// every field the C does *not* copy, each different from [`listener_options`]'s. A copy that
/// touched one of them would be visible here.
fn accepted_defaults() -> SocketOptions {
    SocketOptions {
        active: ACTIVE_TRUE,
        active_count: 2,
        debug: 2,
        netns: Some(b"/netns/accepted".to_vec()),
        read_packets: 9,
        line_delim: b';',
        buffer_set: true,
        delay_send: false,
        ..SocketOptions::default()
    }
}

/// The `{packet, …}`/`{packet_size, …}`/`{header, …}` framing group.
#[test]
fn framing_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.packet, 2, "inet.htype (`:11812`)");
    assert_eq!(copy.packet_size, 100, "inet.psize (`:11813`)");
    assert_eq!(copy.header, 1, "inet.hsz (`:11815`)");
    // The child's own values were all defaults, so "inherited" has to mean "changed".
    assert_ne!(child.packet, copy.packet);
    assert_ne!(child.packet_size, copy.packet_size);
    assert_ne!(child.header, copy.header);
}

/// The `{mode, …}`/`{deliver, …}`/`{exit_on_close, …}` representation group.
#[test]
fn representation_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.mode, MODE_BINARY, "inet.mode (`:11809`)");
    assert_eq!(copy.deliver, DELIVER_PORT, "inet.deliver (`:11811`)");
    assert_eq!(copy.exit_on_close, 0, "inet.exitf (`:11810`)");
    assert_ne!(child.mode, copy.mode);
    assert_ne!(child.deliver, copy.deliver);
    assert_ne!(child.exit_on_close, copy.exit_on_close);
}

/// The `{buffer, …}`/`{high_watermark, …}`/`{low_watermark, …}` window group.
#[test]
fn window_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.buffer_size, 4096, "inet.bufsz (`:11816`)");
    assert_eq!(copy.high, 4096, "desc->high (`:11817`)");
    assert_eq!(copy.low, 2048, "desc->low (`:11818`)");
    assert_ne!(child.buffer_size, copy.buffer_size);
    assert_ne!(child.high, copy.high);
    assert_ne!(child.low, copy.low);
}

/// The `{send_timeout, …}`/`{send_timeout_close, …}` timeout group.
#[test]
fn timeout_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.send_timeout, 500, "desc->send_timeout (`:11819`)");
    assert_eq!(
        copy.send_timeout_close, 1,
        "desc->send_timeout_close (`:11820`)"
    );
    assert_ne!(child.send_timeout, copy.send_timeout);
    assert_ne!(child.send_timeout_close, copy.send_timeout_close);
}

/// The `SHOW_ECONNRESET`/`NO_READ_AHEAD` flag group (the only two `tcp_add_flags` bits the C keeps).
#[test]
fn flag_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert!(
        copy.show_econnreset,
        "tcp_add_flags & TCP_ADDF_SHOW_ECONNRESET (`:11821`)"
    );
    assert!(
        !copy.read_ahead,
        "tcp_add_flags & TCP_ADDF_NO_READ_AHEAD (`:11822`)"
    );
    assert_ne!(child.show_econnreset, copy.show_econnreset);
    assert_ne!(child.read_ahead, copy.read_ahead);
}

/// The busy-msgq limits, which the C reads off the parent port and writes to the child
/// (`erl_drv_busy_msgq_limits`, `:11835`-`:11839`).
#[test]
fn msgq_watermarks_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.msgq_high, 1000, "the parent's busy-msgq high limit");
    assert_eq!(copy.msgq_low, 500, "the parent's busy-msgq low limit");
    assert_ne!(child.msgq_high, copy.msgq_high);
    assert_ne!(child.msgq_low, copy.msgq_low);
}

/// The negative half: every field the C resets (or never copies) keeps the **child's** value, never
/// the listener's.
#[test]
fn driver_only_options_are_never_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);

    // `inet_start` resets `{active, …}` to `INET_PASSIVE` (`:10929`) and the copy list omits it, so
    // an accepted socket starts passive — `prim_inet:accept_opts/3` re-applies the mode.
    assert_eq!(copy.active, ACTIVE_TRUE, "the child's own `active`");
    assert_ne!(copy.active, parent.active);
    // ... and its `{active, N}` counter along with it (`:10930`).
    assert_eq!(copy.active_count, 2, "the child's own `active_count`");
    assert_ne!(copy.active_count, parent.active_count);
    // `inet_start` sets `DDBG_DEFAULT` (`:10983`).
    assert_eq!(copy.debug, 2, "the child's own `debug`");
    assert_ne!(copy.debug, parent.debug);
    // `inet_start` sets `NULL` (`:10982`).
    assert_eq!(
        copy.netns.as_deref(),
        Some(&b"/netns/accepted"[..]),
        "the child's own `netns`"
    );
    assert_ne!(copy.netns, parent.netns);
    // A `udp_descriptor` field (`:1536`) `tcp_inet_copy` never touches.
    assert_eq!(copy.read_packets, 9, "the child's own `read_packets`");
    assert_ne!(copy.read_packets, parent.read_packets);
    // `inet_start` sets `'\n'` (`:10934`); `desc->delimiter` is not in the copy list.
    assert_eq!(copy.line_delim, b';', "the child's own `line_delim`");
    assert_ne!(copy.line_delim, parent.line_delim);
    // `INET_FLG_BUFFER_SET` lives in `desc->flags`, zeroed at `:10981`.
    assert!(copy.buffer_set, "the child's own `buffer_set`");
    assert_ne!(copy.buffer_set, parent.buffer_set);
    // `TCP_ADDF_DELAY_SEND` is outside the mask (`:11821`-`:11823`) — `prim_inet:accept_opts/3`'s
    // half, not the driver's.
    assert!(!copy.delay_send, "the child's own `delay_send`");
    assert_ne!(copy.delay_send, parent.delay_send);
}

/// The whole mapping at once: the result is exactly `listener_options()` for the inherited fields
/// and `accepted_defaults()` for the rest — no field anywhere else.
///
/// `SocketOptions` derives `Debug` over all of its fields, so comparing the rendered stores is a
/// total check (and adding a field to the store without deciding its half breaks this suite).
#[test]
fn the_copy_is_exactly_the_c_mapping() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);

    let expected = SocketOptions {
        // inherited — from the listener
        packet: parent.packet,
        packet_size: parent.packet_size,
        header: parent.header,
        mode: parent.mode,
        deliver: parent.deliver,
        exit_on_close: parent.exit_on_close,
        buffer_size: parent.buffer_size,
        high: parent.high,
        low: parent.low,
        send_timeout: parent.send_timeout,
        send_timeout_close: parent.send_timeout_close,
        show_econnreset: parent.show_econnreset,
        read_ahead: parent.read_ahead,
        msgq_high: parent.msgq_high,
        msgq_low: parent.msgq_low,
        // not inherited — the child's own
        active: child.active,
        active_count: child.active_count,
        debug: child.debug,
        netns: child.netns.clone(),
        read_packets: child.read_packets,
        line_delim: child.line_delim,
        buffer_set: child.buffer_set,
        delay_send: child.delay_send,
    };
    assert_eq!(format!("{copy:?}"), format!("{expected:?}"));
    // The two stores really do differ everywhere, so the check above is not an identity.
    assert_ne!(format!("{parent:?}"), format!("{child:?}"));
}

/// Two default stores copy to a default store: an inheritance with nothing to inherit is the
/// identity, so the accepted default cannot drift away from `inet_start`'s own initializers.
#[test]
fn a_default_listener_yields_the_defaults() {
    let parent = SocketOptions::default();
    let child = SocketOptions::default();
    let copy = child.inherited_from(&parent);
    assert_eq!(format!("{copy:?}"), format!("{parent:?}"));
}

/// [`TcpInetDriver::from_accepted`] adopts through [`TcpInetDriver::from_socket`] first, so an
/// unusable descriptor is that adoption's typed `BeamError::Io` — nothing is inherited and no
/// driver is produced (the caller keeps `fd` to close).
#[test]
fn from_accepted_propagates_the_adoption_failure() {
    let listener = TcpInetDriver::new();
    match TcpInetDriver::from_accepted(-1, &listener) {
        Ok(_) => panic!("fd -1 is not a socket: the adoption must fail"),
        Err(BeamError::Io(err)) => assert_eq!(err.raw_os_error(), Some(libc::EBADF)),
        Err(other) => panic!("expected the typed I/O error of the failed probe, got {other:?}"),
    }
}
