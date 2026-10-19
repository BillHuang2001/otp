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

//! The C's **"block the caller until the port is idle, then re-issue the command"** arm
//! (contract A.4), shared by `erlang:port_command/2,3` and `erlang:send/2,3`.
//!
//! `erts_port_output` answers `ERTS_PORT_OP_BUSY` when a driver has declared its port busy
//! (`io.c:1763-1769`), and every C caller that did not pass `nosuspend` then does the same two
//! things: it **suspends** the calling process (`erts_suspend`, `erl_bif_port.c:220-231` for
//! `port_command`, `bif.c:2578-2582` for `send`) and resumes it when the driver clears the bit
//! (`erts_port_resume_procs`), whereupon the BIF is **re-run** and — the port now being idle —
//! really writes the command. This module is that wait, for a runtime whose BIF seam has no
//! engine-side suspend.
//!
//! # Where the wait can run at all
//!
//! Re-testing the busy bit alone would spin: the bit is cleared by *someone else* — the driver
//! from one of its own callbacks (a drive point the port round runs), or another process calling
//! `port_control/3`. Somebody must therefore drive the machine while the caller waits, and that is
//! exactly what [`Runtime::pump_scheduler`] does for a context that owns the whole pass:
//!
//! * one **port round** ([`Runtime::deliver_port_messages`] inside the pass's own
//!   `scheduler_pass`), which is where a driver's `output`/`ready_output`/`timeout` callbacks run
//!   and where the port's `{Port, {data, …}}` replies are delivered to their owners — the round
//!   never loses an event, because a delivered message is a message its owner will find in its
//!   mailbox;
//! * one **run queue** round, so a process that is going to clear the bit (`timer:apply_after`
//!   reaching `erlang:port_control/3`, say) really runs;
//! * the **timer heap**, so a deadline that frees that process actually fires.
//!
//! A pass with nothing to run also *sleeps* rather than spins: the port round blocks in the
//! poller for up to [`PORT_POLL_SLICE`](crate::port::PORT_POLL_SLICE) while a port is live, and
//! the pass's own timer wait sleeps up to `TIMER_PUMP_SLICE`. So one iteration of the loop always
//! either makes progress or waits — never a busy-spin.
//!
//! A context that may **not** pump ([`RuntimeContext::may_pump_scheduler`] is `false`: a compiled
//! process running inside a scheduler slice, `crate::runner`) cannot re-enter the scheduler, and
//! this runtime has no engine-side BIF-suspend channel through which the process could park and be
//! re-run. That caller therefore gets [`IdleWait::CannotPump`] immediately and answers the typed
//! capability gap — a documented residual (see `M1_GAPS.md` §*Ports, drivers and the send family
//! (A.4)*), never a fabricated `true`.
//!
//! [`RuntimeContext::may_pump_scheduler`]: crate::context::RuntimeContext

use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_common::{BeamError, Result};
use beam_io::{PortCommandOptions, PortCommandOutcome};
use beam_process::{PortId, ProcessId};
use beam_term::Eterm;

use crate::error;
use crate::Runtime;

/// How long a caller may keep waiting for a busy port to go idle.
///
/// The C is **unbounded**: a suspended process stays suspended until the driver clears the bit,
/// however long that takes (a driver that never clears it blocks the caller for ever). A
/// single-threaded runtime that owns its own pass cannot afford that — it would hang the whole
/// executable, and with it every other process — so the wait is bounded and an expired deadline is
/// the typed capability gap [`DEADLINE_REASON`], which names the deadline. Five seconds is far
/// longer than any real driver holds a port busy and far shorter than a user would tolerate.
pub(crate) const BUSY_PORT_DEADLINE: Duration = Duration::from_secs(5);

/// How many scheduler slices one iteration of the wait may run.
///
/// Each iteration is one bounded [`Runtime::pump_scheduler`] step, so this is the same kind of
/// budget the `-eval` `receive` pump uses ([`RECEIVE_PUMP_SLICES`](crate::eval), 64): enough for a
/// helper process to run to its own blocking point or for the clearing `port_control/3` to be
/// served, small enough that the deadline is re-checked promptly.
pub(crate) const BUSY_PORT_PUMP_SLICES: u64 = 64;

/// The capability-gap reason for a wait whose deadline expired with the port still busy.
pub(crate) const DEADLINE_REASON: &str =
    "port_command on a busy port: the port was still busy after the 5 s wait, so the caller was \
     not resumed (the C suspends it for ever)";

/// The capability-gap reason for a caller that owns no scheduler pass.
pub(crate) const NO_PUMP_REASON: &str =
    "port_command on a busy port: this caller runs inside a scheduler slice and cannot re-enter \
     the scheduler to wait for the port (the runtime has no engine-side BIF-suspend channel that \
     would park it and re-run the BIF)";

/// The capability-gap reason for a port that reported busy **again** immediately after the wait
/// observed it idle.
///
/// Unreachable in this single-threaded runtime — nothing runs between the idle verdict and the
/// re-issue, so only a driver whose `is_busy` answer is not stable across two calls could produce
/// it — but reported as a typed gap rather than asserted away, because a fabricated `true` would
/// be a wrong result.
pub(crate) const BUSY_AGAIN_REASON: &str =
    "port_command on a busy port: the port reported busy again right after the wait observed it \
     idle";

/// What one command attempt produced once the C's suspend-and-retry loop has run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortWrite {
    /// The bytes were queued, the round ran and the driver was given them — the C's
    /// `ERTS_PORT_OP_DONE`, which `port_command` renders as `true` and `send` as the message.
    Written,
    /// The port was busy and the caller asked `nosuspend`: nothing was queued and nothing was
    /// written — the C's `ERTS_PORT_OP_BUSY` under `ERTS_PORT_SIG_FLG_NOSUSPEND`, which
    /// `port_command/3` renders as the atom `false` and `send/3` as the atom `nosuspend`.
    Suspended,
}

/// What the **send** route's port arm produced — [`PortWrite`] plus the one verdict only a `send`
/// can produce.
///
/// `erlang:port_command`'s refusals are raises (`badarg`), so [`issue`] has no `Dropped` verdict;
/// a `send`, by contrast, reports the C's `ERTS_PORT_OP_BADARG`/`ERTS_PORT_OP_DROPPED` as a
/// **silent success** (`bif.c:2594-2600`), which is exactly this variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortSend {
    /// The port axis accepted the message: bytes written, or the port closed by `{Pid, close}`.
    Write(PortWrite),
    /// The port axis dropped the message: a port the runtime no longer holds, a payload that is
    /// not iodata, or a message shape the C's `erts_port_command` does not decompose into a
    /// command — the C's `ERTS_PORT_OP_BADARG`/`ERTS_PORT_OP_DROPPED`, both of which leave
    /// `do_send`'s `port_common` block with `ret_val = 0`, so the BIF reports success and
    /// nothing was written.
    Dropped,
}

/// What the C's `erts_port_command` (`io.c:3981-4013`) decomposes a `send` message into.
///
/// `do_send`'s `port_common` block (`bif.c:2551-2613`) does **not** write the message it was
/// given: it hands it to `erts_port_command`, which recognises exactly two shapes and treats
/// everything else as a *bad port signal* — logged and dropped, with the BIF reporting success
/// (`bad_port_signal` → `ERTS_PORT_OP_BADARG`, `bif.c:2594-2600`). Measured on the system oracle
/// (erts-17.1 / OTP 29.1.1) with the `tests/fixtures/busy_driver.c` echo driver: on an **idle**
/// port `Port ! {self(), {command, <<"a">>}}` writes `<<"a">>` (the driver echoes it) while
/// `Port ! <<"a">>` writes **nothing**, and both report the message term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortMessage {
    /// `{Pid, {command, Data}}` with an **internal** pid — the one data shape the C writes
    /// (`erts_port_command`'s `am_command` arm), with `Data` handed on to `erts_port_output`.
    Command(Eterm),
    /// `{Pid, close}` with an internal pid — the C's `erts_port_exit(…, am_normal, …)` arm.
    Close,
    /// Every other message: the C's `bad_port_signal`. Nothing is written and the send reports
    /// success (the message term for `send/2`, `ok` for `send/3`).
    Drop,
}

/// What the wait observed. Only [`IdleWait::Idle`] lets the command be re-issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleWait {
    /// The port is idle: the command may be re-issued.
    Idle,
    /// The port is **gone** — closed, or reaped by its own driver, during the wait. The C's re-run
    /// would find `pt == NULL`, so `port_command` is `badarg` and `send/2` is the silent success.
    Gone,
    /// The deadline expired with the port still busy.
    Deadline,
    /// The caller may not re-enter the scheduler, so it cannot wait at all.
    CannotPump,
}

/// Issues one port command, applying the C's suspend-and-retry loop exactly as
/// `erlang:port_command/2,3` and `erlang:send/2,3` do.
///
/// `make_data` is the caller's iodata conversion. It is a `Fn` (not `FnOnce`) because a busy port
/// makes the command be issued **twice**, and the C too converts the data only on the attempt that
/// is actually accepted (`io.c:1749-1754` before `:1790`) — the busy test precedes the conversion,
/// so a refused attempt never pays for it.
///
/// `owner` is the process the port-command seam records on a port it opens; it is inert here (this
/// seam opens nothing) and is passed only so the seam is built the ordinary way.
///
/// # Errors
///
/// Whatever the command seam reports: [`BeamError::BadArg`] for a port the runtime does not hold
/// (closed, or never opened) and for non-iodata `Data`, the `notsup` raise for `[force]` on a
/// driver without `ERL_DRV_FLAG_SOFT_BUSY`, and the write's own failure (a child that closed its
/// stdin is `EPIPE`); plus the three capability gaps above — a deadline expiry, a caller that
/// cannot pump, and a port that reported busy again after the wait.
pub(crate) fn issue<F>(
    runtime: &mut Runtime,
    may_pump: bool,
    owner: ProcessId,
    id: PortId,
    make_data: F,
    options: PortCommandOptions,
) -> Result<PortWrite>
where
    F: Fn() -> Result<Vec<u8>>,
{
    match attempt(runtime, owner, id, &make_data, options)? {
        PortCommandOutcome::Done => Ok(PortWrite::Written),
        PortCommandOutcome::Busy if options.is_nosuspend() => Ok(PortWrite::Suspended),
        PortCommandOutcome::Busy => {
            match wait_until_idle(runtime, may_pump, id)? {
                IdleWait::Idle => {}
                IdleWait::Gone => return Err(BeamError::BadArg),
                IdleWait::Deadline => return Err(error::unsupported(DEADLINE_REASON)),
                IdleWait::CannotPump => return Err(error::unsupported(NO_PUMP_REASON)),
            }
            match attempt(runtime, owner, id, &make_data, options)? {
                PortCommandOutcome::Done => Ok(PortWrite::Written),
                PortCommandOutcome::Busy => Err(error::unsupported(BUSY_AGAIN_REASON)),
            }
        }
    }
}

/// One command attempt through the runtime's port seam — no waiting, no retry.
///
/// The decision itself is [`beam_io`]'s ([`PortRegistry::command_with_data`], whose
/// [`crate::port::PortRegistry::command`] owns the write round, the `{packet, N}` framing, the
/// byte accounting and the notification fold); this only builds the hook.
///
/// [`PortRegistry::command_with_data`]: crate::port::PortRegistry::command_with_data
fn attempt<F>(
    runtime: &mut Runtime,
    owner: ProcessId,
    id: PortId,
    make_data: &F,
    options: PortCommandOptions,
) -> Result<PortCommandOutcome>
where
    F: Fn() -> Result<Vec<u8>>,
{
    let atoms = runtime.code_server.atoms_handle();
    let mut hook = crate::port::RuntimePorts::new(Rc::clone(&runtime.ports), atoms, owner);
    hook.command_with_data(id, make_data, options)
}

/// Waits until `id` is **idle**, driving the machine while it waits.
///
/// Each iteration re-tests the port's state first, so a port that is already idle (or already gone)
/// costs no round at all, and then runs one bounded [`Runtime::pump_scheduler`] step — the pass
/// that runs the port round, the run queue and the timer heap (see the module docs for why that is
/// the whole of the wait, and why it cannot busy-spin).
///
/// # Errors
///
/// As [`Runtime::pump_scheduler`]: the process table's delivery/termination errors, the port
/// poller's/driver's failures, and a nested borrow of the port table (a typed capability gap).
pub(crate) fn wait_until_idle(
    runtime: &mut Runtime,
    may_pump: bool,
    id: PortId,
) -> Result<IdleWait> {
    wait_until_idle_by(runtime, may_pump, id, Instant::now() + BUSY_PORT_DEADLINE)
}

/// [`wait_until_idle`] against an explicit deadline — the form the unit tests use, so a deadline
/// expiry is exercised in milliseconds instead of seconds.
///
/// # Errors
///
/// As [`wait_until_idle`].
fn wait_until_idle_by(
    runtime: &mut Runtime,
    may_pump: bool,
    id: PortId,
    deadline: Instant,
) -> Result<IdleWait> {
    loop {
        match port_state(runtime, id)? {
            // The port is gone: `port_command` is `badarg` and `send/2` is the silent success.
            None => return Ok(IdleWait::Gone),
            Some(false) => return Ok(IdleWait::Idle),
            Some(true) => {}
        }
        // A caller inside a scheduler slice may not re-enter the scheduler, and there is no
        // engine-side BIF suspend to park it: report the blocker rather than spin or lie.
        if !may_pump {
            return Ok(IdleWait::CannotPump);
        }
        if Instant::now() >= deadline {
            return Ok(IdleWait::Deadline);
        }
        runtime.pump_scheduler(BUSY_PORT_PUMP_SLICES)?;
    }
}

/// The port's busy state: `None` when the runtime does not hold `id`, else whether it is busy.
///
/// Both facts come from the registry's own metadata (`PortRegistry::contains`/`is_busy`), i.e. the
/// same state [`crate::port::PortRegistry::decide_before_data`] decides the command from, so the
/// wait can never disagree with the attempt that follows it.
fn port_state(runtime: &Runtime, id: PortId) -> Result<Option<bool>> {
    let registry = runtime
        .ports
        .try_borrow()
        .map_err(|_| error::unsupported("the port table is already in use"))?;
    if !registry.contains(id) {
        return Ok(None);
    }
    Ok(Some(registry.is_busy(id)))
}

/// The C's **`send` port arm** (`bif.c:2551-2613`): decompose the message the way
/// `erts_port_command` does, write the one data shape, and report every failure as a silent
/// success.
///
/// `do_send`'s `port_common` block hands the message to `erts_port_command`
/// (`io.c:3981-4013`), which recognises exactly two shapes — `{Pid, {command, Data}}` (write
/// `Data`, `erts_port_output`'s busy/nosuspend matrix and all) and `{Pid, close}`
/// (`erts_port_exit`) — and treats **everything else** as a bad port signal: the C logs it and
/// drops it, and the block leaves with `ret_val = 0`, so the BIF reports success and nothing was
/// written. A dead / never-opened / wrong-incarnation port id never even enters the block
/// (`pt == NULL`), which is the same silent success — measured on the oracle: after
/// `port_close(P)`, `erlang:send(P, hello)` answers `hello`, while `port_command(P, <<"x">>)`
/// answers `error:badarg`.
///
/// A capability gap (an expired deadline, a caller that cannot pump, a port busy again after the
/// wait) is **not** swallowed: it is a raise the caller cannot observe as a value, so it is
/// propagated exactly as [`issue`] reports it.
///
/// # Errors
///
/// Only the capability gaps of [`issue`] and a genuine write failure (a child that closed its
/// stdin is `EPIPE`, which the C reports through the port's failure channel rather than the BIF).
pub(crate) fn send_message_to_port(
    runtime: &mut Runtime,
    may_pump: bool,
    owner: ProcessId,
    id: PortId,
    message: Eterm,
    nosuspend: bool,
) -> Result<PortSend> {
    match classify_message(runtime, message) {
        // The C's `bad_port_signal`: logged and dropped, and the send reports success.
        PortMessage::Drop => Ok(PortSend::Dropped),
        // `erts_port_exit(…, am_normal, …)`: the port axis accepted the message by closing it,
        // and a port that is already gone is the block's silent success.
        PortMessage::Close => match close_port(runtime, owner, id) {
            Ok(()) => Ok(PortSend::Write(PortWrite::Written)),
            Err(BeamError::BadArg) => Ok(PortSend::Dropped),
            Err(other) => Err(other),
        },
        // `erts_port_output` behind `erts_port_command`: the whole `port_command` matrix,
        // including the C's suspend-and-retry loop, with the send route's silent failures.
        PortMessage::Command(data) => {
            let options = PortCommandOptions::EMPTY.with_nosuspend(nosuspend);
            let written = issue(runtime, may_pump, owner, id, || data_bytes(data), options);
            match written {
                // `DONE`, and the `nosuspend` arm: the caller folds the verdict into its answer.
                Ok(write) => Ok(PortSend::Write(write)),
                // The C's silent success for a port that is not there and for whatever the port
                // axis rejected (a non-iodata `Data` is `bad_value` → `bad_port_signal` there).
                Err(BeamError::BadArg) => Ok(PortSend::Dropped),
                Err(other) => Err(other),
            }
        }
    }
}

/// One payload's iodata conversion, in the shape [`issue`] rebuilds on each attempt.
///
/// The C converts the iolist **after** the busy test (`io.c:1749-1754` against `:1790`), which is
/// why the conversion is a closure there and here: a refused attempt never pays for it.
fn data_bytes(data: Eterm) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    crate::native::port::iodata_bytes(data, &mut bytes)?;
    Ok(bytes)
}

/// Classifies a `send` message exactly as `erts_port_command` (`io.c:3981-4013`) does.
///
/// The C's tests, in its own order: the message must be a 2-tuple; its first element must be an
/// **internal** pid (`is_internal_pid`); and its second element is then either the atom `close`,
/// a 2-tuple `{command, Data}`, or something the block cannot use. The atom comparisons are by
/// **name** through the runtime's one shared table, so a message built on another heap/module
/// classifies identically.
fn classify_message(runtime: &Runtime, message: Eterm) -> PortMessage {
    let pair = || {
        if beam_engine::terms::tuple_arity(message).ok()? != 2 {
            return None;
        }
        let from = beam_engine::terms::tuple_element(message, 0).ok()?;
        let rest = beam_engine::terms::tuple_element(message, 1).ok()?;
        Some((from, rest))
    };
    let Some((from, rest)) = pair() else {
        return PortMessage::Drop;
    };
    if !ProcessId::is_process(from) {
        // A remote pid (or any other term) as the "from": `bad_port_signal`.
        return PortMessage::Drop;
    }
    if runtime.atom_name_of(rest) == Some("close") {
        return PortMessage::Close;
    }
    if beam_engine::terms::tuple_arity(rest).ok() == Some(2) {
        let command = || {
            let tag = beam_engine::terms::tuple_element(rest, 0).ok()?;
            let data = beam_engine::terms::tuple_element(rest, 1).ok()?;
            Some((tag, data))
        };
        if let Some((tag, data)) = command() {
            if runtime.atom_name_of(tag) == Some("command") {
                return PortMessage::Command(data);
            }
        }
    }
    PortMessage::Drop
}

/// Closes `id` through the runtime's port seam — the C's `erts_port_exit` arm of
/// `erts_port_command`.
///
/// A port the runtime no longer holds is [`BeamError::BadArg`], which [`send_message_to_port`]
/// folds into the block's silent success (the C's `pt == NULL`).
fn close_port(runtime: &mut Runtime, owner: ProcessId, id: PortId) -> Result<()> {
    use beam_bifs::PortHook;

    let atoms = runtime.code_server.atoms_handle();
    let mut hook = crate::port::RuntimePorts::new(Rc::clone(&runtime.ports), atoms, owner);
    hook.close_port(id)
}
