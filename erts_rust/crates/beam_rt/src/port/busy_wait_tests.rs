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

//! The **busy-port wait** and the **port-signal decomposition**, unit-level.
//!
//! `src/port/busy_wait.rs` owns two things the oracle can only partly reach: the C's
//! suspend-and-retry loop (`issue`/`wait_until_idle`, `io.c:1731-1790`) and
//! `erts_port_command`'s message decomposition (`classify_message`, `io.c:3981-4013`). The
//! end-to-end suite (`tests/port_busy.rs`) drives both through a **real** C driver, which is what
//! proves a driver's `set_busy_port`/`driver_flags` cross the A.4 boundary — but an end-to-end run
//! cannot produce the *refusals* cheaply: a deadline is 5 s, an unknown port id has no term, and a
//! caller that may not pump exists only inside a scheduler slice.
//!
//! These tests therefore drive the production functions directly over
//! [`register_busy_port`](crate::port::test_support::register_busy_port) — a registered driver whose
//! busy state, `ERL_DRV_FLAG_SOFT_BUSY` claim and `output`-callback **write counter** the test
//! controls — so every row can assert what the C makes observable: the verdict, the reason, and
//! whether the driver's `output` callback ran at all.
//!
//! # The table (C reference in brackets)
//!
//! | port | `may_pump` | options | wait verdict | `issue` | writes |
//! |---|---|---|---|---|---|
//! | idle | either | none | `Idle` | `Written` | 1 (the bytes) |
//! | busy | — | — | `Gone` (unknown id) | `error:badarg` | 0 |
//! | busy | `true`/`false` | none, `[nosuspend]` | `CannotPump` | the `NO_PUMP_REASON` gap | 0 |
//! | busy | `true` | none, expired deadline | `Deadline` | the `DEADLINE_REASON` gap | 0 |
//! | cleared | either | none | `Idle` | `Written` | 1 (the bytes) |
//! | busy | either | `[nosuspend]` | — | `Suspended` (no conversion) | 0 |
//! | either | either | `[force]` without `SOFT_BUSY` | — | `error:notsup` | 0 |
//!
//! The two capability gaps are asserted by **identity** with the module's own constants, not by
//! substring: the reason a caller reports is the one the module names
//! (`crate::error::unsupported_reason` extracts it from the raise).
use std::cell::Cell;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::PortCommandOptions;
use beam_engine::terms;
use beam_process::{PortId, ProcessId};
use beam_term::{signed_value, Eterm};

use crate::error;
use crate::native::test_support::{atom, call, chars, runtime as native_runtime};
use crate::port::test_support::register_busy_port;
use crate::port::tests::{raised_text, runtime_tuple};
use crate::runtime::Runtime;

use super::{
    classify_message, issue, send_message_to_port, wait_until_idle, wait_until_idle_by, IdleWait,
    PortMessage, PortSend, PortWrite, DEADLINE_REASON, NO_PUMP_REASON,
};

/// The pid the fixture ports belong to — the same owner `register_busy_port` registers, which the
/// port-command seam records on a port it opens (inert here: [`issue`] opens nothing).
const OWNER: ProcessId = ProcessId::new(1, 0);

/// A payload closure that **records** whether the iodata conversion ran, and answers `bytes`.
///
/// The C converts the iolist only on the attempt the port actually accepts (`io.c:1749-1754`
/// before `:1790`), so "the closure did not run" is the observable form of "the command was
/// refused before the conversion".
fn data<'a>(ran: &'a Cell<bool>, bytes: &'static [u8]) -> impl Fn() -> beam_common::Result<Vec<u8>> + 'a {
    move || {
        ran.set(true);
        Ok(bytes.to_vec())
    }
}

/// `port_info(Port, output)` — the port's own accepted-byte counter, i.e. the runtime's figure for
/// "the bytes really were written" (as opposed to "the driver was called with nothing").
fn output_counter(runtime: &mut Runtime, port: Eterm) -> i64 {
    let item = atom(runtime, "output");
    let answer = call(runtime, "erlang", "port_info", &[port, item]).expect("port_info/2");
    let value = terms::tuple_element(answer, 1).expect("the {output, N} pair");
    signed_value(value)
}

/// An **idle** port is waitable at once and its command is written: `Idle` without a single
/// scheduler pass, `Written`, the driver's `output` callback ran exactly once, and the port's own
/// `output` counter is the payload's byte length.
#[test]
fn an_idle_port_is_waitable_and_its_command_is_written() {
    let mut runtime = native_runtime();
    let (port, _flag, writes) = register_busy_port(&mut runtime, "bw_idle", false, false);
    assert_eq!(
        wait_until_idle(&mut runtime, false, port).expect("an idle port is readable"),
        IdleWait::Idle,
        "an idle port is waitable without pumping the scheduler"
    );
    let ran = Cell::new(false);
    let written = issue(
        &mut runtime,
        false,
        OWNER,
        port,
        data(&ran, b"xy"),
        PortCommandOptions::EMPTY,
    )
    .expect("an idle port accepts the command");
    assert_eq!(written, PortWrite::Written);
    assert!(ran.get(), "an accepted command converts its payload");
    assert_eq!(writes.get(), 1, "the driver's `output` ran exactly once");
    assert_eq!(
        output_counter(&mut runtime, port.to_eterm()),
        2,
        "the port's `output` counter is the payload's byte length"
    );
}

/// A port the runtime does not hold is `Gone` — and `issue` answers the C's `badarg`
/// (`sig_lookup_port`), **before** any payload conversion.
#[test]
fn a_port_the_registry_does_not_hold_is_gone_and_badarg() {
    let mut runtime = native_runtime();
    // A port number nothing registered: `PortRegistry::contains` is false, which is the C's
    // `pt == NULL` / `ERTS_PORT_SFLGS_INVALID_LOOKUP`.
    let missing = PortId::new(9_999_999);
    assert_eq!(
        wait_until_idle(&mut runtime, true, missing).expect("the port table is readable"),
        IdleWait::Gone
    );
    let ran = Cell::new(false);
    let error = issue(
        &mut runtime,
        true,
        OWNER,
        missing,
        data(&ran, b"x"),
        PortCommandOptions::EMPTY,
    )
    .expect_err("a port the runtime does not hold cannot be written");
    assert!(
        matches!(error, BeamError::BadArg),
        "the C's `sig_lookup_port` NULL is badarg, got {error}"
    );
    assert!(
        !ran.get(),
        "the refusal precedes the iodata conversion (`io.c:1749-1754`)"
    );
}

/// A caller that owns no scheduler pass cannot wait: the wait answers `CannotPump` immediately and
/// `issue` reports the module's own capability gap, with **nothing written**.
///
/// This is the arm `tests/port_busy.rs` pins end to end on the `run` route (exit 3); here it is the
/// verdict and the reason identity, without a scheduler slice.
#[test]
fn a_busy_port_a_caller_cannot_wait_for_is_the_typed_gap() {
    let mut runtime = native_runtime();
    let (port, _flag, writes) = register_busy_port(&mut runtime, "bw_nopump", true, false);
    assert_eq!(
        wait_until_idle(&mut runtime, false, port).expect("the port table is readable"),
        IdleWait::CannotPump,
        "a caller inside a scheduler slice cannot re-enter the scheduler"
    );
    let ran = Cell::new(false);
    let error = issue(
        &mut runtime,
        false,
        OWNER,
        port,
        data(&ran, b"x"),
        PortCommandOptions::EMPTY,
    )
    .expect_err("a busy port cannot be written by a caller that cannot wait");
    assert_eq!(
        error::unsupported_reason(&error),
        Some(NO_PUMP_REASON.to_string()),
        "the gap is the module's own reason, never a fabricated `true`"
    );
    assert!(!ran.get(), "nothing was converted");
    assert_eq!(writes.get(), 0, "nothing reached the driver's `output`");
}

/// An expired deadline is `Deadline`, and the command is the module's `DEADLINE_REASON` gap: the C
/// suspends the caller for ever, while this runtime bounds the wait and reports the blocker.
///
/// The `Deadline` half is driven through the private deadline form with a deadline already in the
/// past (no wait at all); the `issue` half is the real bounded wait, which is why it takes
/// [`BUSY_PORT_DEADLINE`](super::BUSY_PORT_DEADLINE) seconds — the wait sleeps in the port round
/// rather than spinning.
#[test]
fn an_expired_deadline_is_the_typed_gap() {
    let mut runtime = native_runtime();
    let (port, _flag, writes) = register_busy_port(&mut runtime, "bw_deadline", true, false);
    let past = Instant::now() - Duration::from_millis(1);
    assert_eq!(
        wait_until_idle_by(&mut runtime, true, port, past).expect("the port table is readable"),
        IdleWait::Deadline,
        "a deadline that has already passed is not waited on"
    );
    let ran = Cell::new(false);
    let error = issue(
        &mut runtime,
        true,
        OWNER,
        port,
        data(&ran, b"x"),
        PortCommandOptions::EMPTY,
    )
    .expect_err("the bounded wait expires with the port still busy");
    assert_eq!(
        error::unsupported_reason(&error),
        Some(DEADLINE_REASON.to_string()),
        "the expired wait is the module's own reason"
    );
    assert!(!ran.get(), "no attempt was ever accepted, so nothing was converted");
    assert_eq!(writes.get(), 0, "nothing reached the driver's `output`");
}

/// A busy port that goes idle **before** the wait is idle at once, and the re-issued command is
/// written — the arm a driver's own `set_busy_port(port, 0)` (or another process's
/// `port_control/3`) produces.
///
/// `may_pump` is deliberately `false` here: the port is already idle, so the wait must not need a
/// scheduler pass at all.
#[test]
fn a_busy_port_cleared_before_the_wait_is_writable() {
    let mut runtime = native_runtime();
    let (port, flag, writes) = register_busy_port(&mut runtime, "bw_cleared", true, false);
    flag.set_busy(false);
    assert_eq!(
        wait_until_idle(&mut runtime, false, port).expect("the port table is readable"),
        IdleWait::Idle
    );
    let ran = Cell::new(false);
    let written = issue(
        &mut runtime,
        false,
        OWNER,
        port,
        data(&ran, b"xyz"),
        PortCommandOptions::EMPTY,
    )
    .expect("the cleared port accepts the command");
    assert_eq!(written, PortWrite::Written);
    assert!(ran.get());
    assert_eq!(writes.get(), 1, "the driver's `output` ran exactly once");
    assert_eq!(output_counter(&mut runtime, port.to_eterm()), 3);
}

/// `[nosuspend]` on a busy port is `Suspended`: nothing is queued, nothing is written, and the
/// payload is **never converted** — not even when it is not iodata at all, because the busy arm
/// precedes the conversion (`io.c:1749-1754` vs `:1790`, the row
/// `src/port/command_matrix_tests.rs` also pins through the BIF).
#[test]
fn nosuspend_on_a_busy_port_writes_nothing_and_converts_nothing() {
    let mut runtime = native_runtime();
    let (port, _flag, writes) = register_busy_port(&mut runtime, "bw_nosuspend", true, false);
    let nosuspend = PortCommandOptions::EMPTY.with_nosuspend(true);
    let ran = Cell::new(false);
    let suspended = issue(&mut runtime, true, OWNER, port, data(&ran, b"x"), nosuspend)
        .expect("a busy port under `nosuspend` answers, it does not wait");
    assert_eq!(suspended, PortWrite::Suspended);
    assert!(!ran.get(), "the busy arm precedes the iodata conversion");
    // The same verdict for a payload that could not be converted at all.
    let unconvertible = issue(
        &mut runtime,
        true,
        OWNER,
        port,
        || Err(BeamError::BadArg),
        nosuspend,
    )
    .expect("the payload is never looked at");
    assert_eq!(unconvertible, PortWrite::Suspended);
    assert_eq!(writes.get(), 0, "nothing reached the driver's `output`");
}

/// `[force]` on a driver entry that does **not** advertise `ERL_DRV_FLAG_SOFT_BUSY` is the
/// oracle's `error:notsup` — on an **idle** port as well as a busy one, because the test is a fact
/// about the entry, not about the port's state (`erl_bif_port.c:195-198`).
#[test]
fn force_without_soft_busy_is_notsup_on_an_idle_and_on_a_busy_port() {
    let mut runtime = native_runtime();
    for busy in [false, true] {
        let (port, _flag, writes) = register_busy_port(&mut runtime, "bw_force", busy, false);
        let ran = Cell::new(false);
        let error = issue(
            &mut runtime,
            false,
            OWNER,
            port,
            data(&ran, b"x"),
            PortCommandOptions::EMPTY.with_force(true),
        )
        .expect_err("a driver entry without SOFT_BUSY rejects `force`");
        assert_eq!(
            raised_text(&error),
            "error:notsup",
            "the refusal is notsup (busy = {busy})"
        );
        assert_eq!(
            error::unsupported_reason(&error),
            None,
            "`notsup` is a genuine error, not a capability gap: {error}"
        );
        assert!(!ran.get(), "the refusal precedes the iodata conversion");
        assert_eq!(writes.get(), 0, "nothing reached the driver's `output`");
    }
}

/// `erts_port_command`'s **message decomposition** (`io.c:3981-4013`): only
/// `{Pid, {command, Data}}` and `{Pid, close}` are port signals, with an **internal** pid as the
/// first element; everything else is the C's `bad_port_signal`.
///
/// The `{Pid, {connect, NewPid}}` arm is the C's third shape (`io.c:4006-4009`), and this runtime
/// classifies it as a drop: `src/port/ops.rs::connect` implements the `port_connect/2` **BIF** form
/// (it creates a link, requires a live target and answers no message), while the bang form sets the
/// port's connected pid without a link, accepts a dead pid, requires the message's pid to be the
/// port's own connected process, and replies `{Port, connected}` — a reply that needs a
/// `&mut ProcessTable` the send-to-port seam does not carry. Performing the transfer without that
/// reply would be a well-formed wrong result, so the arm stays the C's *drop* until the transfer and
/// its reply can be served together.
#[test]
fn the_port_signal_decomposition_matches_erts_port_command() {
    let mut runtime = native_runtime();
    let from = ProcessId::new(7, 3).to_eterm();
    let other_pid = ProcessId::new(8, 4).to_eterm();
    let data = chars(&mut runtime, "x");
    let command = atom(&mut runtime, "command");
    let close = atom(&mut runtime, "close");
    let connect = atom(&mut runtime, "connect");
    let unrelated = atom(&mut runtime, "unrelated");
    let command_tag = runtime_tuple(&mut runtime, &[command, data]);
    let connect_tag = runtime_tuple(&mut runtime, &[connect, other_pid]);
    let command_signal = runtime_tuple(&mut runtime, &[from, command_tag]);
    let connect_signal = runtime_tuple(&mut runtime, &[from, connect_tag]);
    let close_signal = runtime_tuple(&mut runtime, &[from, close]);
    let unrelated_pair = runtime_tuple(&mut runtime, &[from, unrelated]);
    let non_pid_from = runtime_tuple(&mut runtime, &[data, close]);
    let port_from = runtime_tuple(&mut runtime, &[PortId::new(1).to_eterm(), close]);
    let three_tuple = runtime_tuple(&mut runtime, &[from, close, close]);
    let unrelated_tag = runtime_tuple(&mut runtime, &[unrelated, data]);
    let unrelated_signal = runtime_tuple(&mut runtime, &[from, unrelated_tag]);

    assert_eq!(
        classify_message(&runtime, command_signal),
        PortMessage::Command(data),
        "`{{Pid, {{command, Data}}}}` is the one data shape the C writes"
    );
    assert_eq!(
        classify_message(&runtime, close_signal),
        PortMessage::Close,
        "`{{Pid, close}}` is the C's `erts_port_exit` arm"
    );
    for (label, message) in [
        ("a plain payload", data),
        ("a two-tuple whose second element is neither shape", unrelated_pair),
        ("a two-tuple whose payload tag is neither shape", unrelated_signal),
        ("a non-pid first element", non_pid_from),
        ("a port as the first element", port_from),
        ("a three-tuple", three_tuple),
    ] {
        assert_eq!(
            classify_message(&runtime, message),
            PortMessage::Drop,
            "`{label}` is the C's `bad_port_signal`"
        );
    }
    assert_eq!(
        classify_message(&runtime, connect_signal),
        PortMessage::Drop,
        "the bang `{{Pid, {{connect, NewPid}}}}` transfer is not performed by this runtime"
    );
}

/// The send route's verdicts over the same decomposition: a port signal on an idle port is
/// `Write(Write::Written)`, `{Pid, close}` really closes the port, and everything else is the C's
/// **silent success** — `Dropped`, nothing written, and the send reports its message.
#[test]
fn the_send_route_writes_signals_and_drops_everything_else() {
    let mut runtime = native_runtime();
    let (port, _flag, writes) = register_busy_port(&mut runtime, "bw_send", false, false);
    let from = OWNER.to_eterm();
    let data = chars(&mut runtime, "abc");
    let command = atom(&mut runtime, "command");
    let close = atom(&mut runtime, "close");
    let command_tag = runtime_tuple(&mut runtime, &[command, data]);
    let command_signal = runtime_tuple(&mut runtime, &[from, command_tag]);
    let close_signal = runtime_tuple(&mut runtime, &[from, close]);
    let verdict = send_message_to_port(&mut runtime, true, OWNER, port, command_signal, false)
        .expect("a signal on an idle port is written");
    assert_eq!(verdict, PortSend::Write(PortWrite::Written));
    assert_eq!(writes.get(), 1);
    assert_eq!(output_counter(&mut runtime, port.to_eterm()), 3);
    let verdict = send_message_to_port(&mut runtime, true, OWNER, port, data, false)
        .expect("a bad port signal is a silent success");
    assert_eq!(
        verdict,
        PortSend::Dropped,
        "the C logs and drops it, and the send reports success"
    );
    assert_eq!(writes.get(), 1, "a dropped message writes nothing");
    // `{Pid, close}` closes the port; a second close on the now-gone port is the block's silent
    // success (`pt == NULL`), never a fabricated `true`.
    let verdict = send_message_to_port(&mut runtime, true, OWNER, port, close_signal, false)
        .expect("a close signal closes the port");
    assert_eq!(verdict, PortSend::Write(PortWrite::Written));
    let verdict = send_message_to_port(&mut runtime, true, OWNER, port, close_signal, false)
        .expect("closing a port that is already gone is the block's silent success");
    assert_eq!(verdict, PortSend::Dropped);
}
