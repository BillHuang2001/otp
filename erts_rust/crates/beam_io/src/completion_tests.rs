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

//! The driver→owner **completion side channel**, pinned structurally.
//!
//! `beam_io` is term-free, so these tests assert *channel contents* — which port a completion is
//! stamped with, in what order the completions come out, and that each is reported exactly once —
//! never the Erlang tuple the runtime builds from them.
//!
//! * [`HookDriver`] delivers one identifiable [`CompletionEvent::InetAsync`] from **every**
//!   driver interaction point (`start`, `output`, `ready_input`, `ready_output`, `control`,
//!   `timeout`, `drive_async`, `port_exit`, `stop`), so one pass over the whole driver surface
//!   pins that the sink is installed everywhere. The `id` of each event is the hook's number, so
//!   the drain's contents and its order are both checkable.
//! * The registry tests pin the runtime-facing half: a round (including the **timer** and
//!   `driver_async` paths, whose own return values cannot carry a reply) reports what its drivers
//!   staged; two ports are drained in port-number order; and a completion staged before a
//!   [`PortRegistry::close`] still reaches the owner exactly once, while the port's *number* is
//!   retired only by the `Closed` report — the existing rule, unpolluted by the completion.
//!
//! Nothing here needs a descriptor, a child process, an `erl` oracle or a poll wait: the driver
//! registers no fd, so the ports are driven directly.

use beam_common::Result;
use beam_process::PortId;

use crate::completion::{CompletionEvent, InetAsyncResult, InetReason, InetReply, PortCompletion};
use crate::driver::{Driver, DriverContext};
use crate::poller::Poller;
use crate::port::{Port, PortCommandOptions, DEFAULT_READINESS_RED_BUDGET};
use crate::port_task::{PortNotification, ReadMessage};
use crate::queue::ByteQueue;
use crate::registry::PortRegistry;
use crate::driver::PortArms;

/// The hook numbers [`HookDriver`] uses as its completion `id`s, in the order the checks in
/// [`every_driver_interaction_point_can_stage_a_completion`] drive them.
mod hook {
    pub(super) const START: u16 = 1;
    pub(super) const OUTPUT: u16 = 2;
    pub(super) const READY_INPUT: u16 = 3;
    pub(super) const READY_OUTPUT: u16 = 4;
    pub(super) const CONTROL: u16 = 5;
    pub(super) const TIMEOUT: u16 = 6;
    pub(super) const DRIVE_ASYNC: u16 = 7;
    pub(super) const PORT_EXIT: u16 = 8;
    pub(super) const STOP: u16 = 9;
}

/// An `{inet_async, Port, Id, ok}` completion, tagged with `id` so a test can tell one hook's
/// delivery from another's.
fn async_ok(id: u16) -> CompletionEvent {
    CompletionEvent::InetAsync {
        id,
        result: InetAsyncResult::Ok,
    }
}

/// Stages one identifiable completion, and is the port's only source of them: the driver
/// registers no descriptor, so nothing is ever polled and every callback below is reached by a
/// direct call.
fn stage(ctx: &mut DriverContext<'_>, id: u16) -> Result<()> {
    ctx.deliver_completion(async_ok(id))
}

/// Drains the port's output queue (so the port's write-accounting check passes) and then stages a
/// completion — the order a real socket driver uses: the bytes go out, then the reply is handed
/// up.
fn write(ctx: &mut DriverContext<'_>, id: u16) -> Result<usize> {
    let len = ctx.output().len();
    ctx.output().consume(len);
    stage(ctx, id)?;
    Ok(len)
}

/// A driver that stages exactly one completion from **every** interaction point it implements.
///
/// It also declares a **one-shot** driver deadline (`Driver::next_timeout`, consumed by its own
/// `timeout` callback), so the port's `TIMEOUT` task is armed the way a real timer-using driver
/// arms it — that is the only route to a timer-driven completion, since the port task that runs
/// `Driver::timeout` discards its `Result`.
struct HookDriver {
    /// The deadline `Driver::next_timeout` declares, cleared by the `timeout` callback that
    /// consumes it.
    deadline: Option<std::time::Instant>,
}

impl HookDriver {
    /// A driver with a deadline that is already due, so the first round runs its timer.
    fn new() -> HookDriver {
        HookDriver {
            deadline: Some(std::time::Instant::now()),
        }
    }
}

impl Driver for HookDriver {
    fn name(&self) -> &str {
        "completion-hook-test"
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        stage(ctx, hook::START)
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        write(ctx, hook::OUTPUT)
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        stage(ctx, hook::READY_INPUT)
    }

    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        write(ctx, hook::READY_OUTPUT)
    }

    fn control(
        &mut self,
        ctx: &mut DriverContext<'_>,
        _op: &str,
        _arg: &[u8],
    ) -> Result<Vec<u8>> {
        stage(ctx, hook::CONTROL)?;
        Ok(Vec::new())
    }

    fn timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // The port task that runs a `TIMEOUT` discards this `Result`, so a completion staged here
        // is the only way a timer-driven reply can reach the owner. The deadline is one-shot.
        self.deadline = None;
        stage(ctx, hook::TIMEOUT)
    }

    fn next_timeout(&self) -> Option<std::time::Instant> {
        self.deadline
    }

    fn drive_async(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        stage(ctx, hook::DRIVE_ASYNC)?;
        Ok(1)
    }

    fn has_pending_async(&self) -> bool {
        true
    }

    fn port_exit(
        &mut self,
        ctx: &mut DriverContext<'_>,
        _reason: crate::driver::PortExitReason,
    ) -> Result<()> {
        stage(ctx, hook::PORT_EXIT)
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        stage(ctx, hook::STOP)
    }
}

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// The `id`s of a drained batch, in drain order — the `InetAsync` shape every test here stages.
fn ids(completions: &[PortCompletion]) -> Vec<u16> {
    completions
        .iter()
        .map(|completion| match &completion.event {
            CompletionEvent::InetAsync { id, .. } => *id,
            other => panic!("this suite stages only `InetAsync` completions, got {other:?}"),
        })
        .collect()
}

// -----------------------------------------------------------------------------------------------
// The channel primitive: `DriverContext::deliver_completion`
// -----------------------------------------------------------------------------------------------

/// A context with no completion sink is the typed `unsupported` naming the missing sink — never a
/// silent drop, because the owner may be blocked in `receive {inet_reply, …}`.
#[test]
fn deliver_completion_without_a_sink_is_the_typed_unsupported() {
    let mut poller = poller();
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut ctx = DriverContext::new(
        PortId::new(7),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    );

    let error = ctx
        .deliver_completion(async_ok(3))
        .expect_err("no sink is installed");
    assert!(
        error.to_string().contains("no completion sink"),
        "the diagnostic names the missing sink: {error}"
    );
}

/// With a sink installed, the context **stamps its own port** — a driver cannot attribute a
/// completion to another port — and each delivery is one completion.
#[test]
fn the_context_stamps_the_completion_with_its_own_port() {
    let mut poller = poller();
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut sink = Vec::new();
    let mut ctx = DriverContext::new(
        PortId::new(7),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    )
    .with_completions(&mut sink);

    ctx.deliver_completion(async_ok(1)).expect("a sink");
    ctx.deliver_completion(CompletionEvent::reply_ok(Some(vec![131, 80, 0])))
        .expect("a sink");

    assert_eq!(
        sink,
        vec![
            PortCompletion::new(PortId::new(7), async_ok(1)),
            PortCompletion::new(
                PortId::new(7),
                CompletionEvent::reply_ok(Some(vec![131, 80, 0]))
            ),
        ],
        "both deliveries land in order, stamped with the context's port"
    );
}

// -----------------------------------------------------------------------------------------------
// Every driver interaction point installs the sink
// -----------------------------------------------------------------------------------------------

/// One pass over the whole driver surface: `start`, `output`, `ready_input`, `ready_output`,
/// `control`, `timeout`, `drive_async`, `port_exit` and `stop` each stage a completion, and the
/// drain reports all nine **exactly once**, in the order the hooks ran, each stamped with the
/// port's own id.
///
/// This is the regression test for "the sink must be reachable from every driver interaction
/// point": a missing `.with_completions(…)` on any one of those paths fails here.
#[test]
fn every_driver_interaction_point_can_stage_a_completion() {
    let mut poller = poller();
    let id = PortId::new(42);
    let mut port = Port::new(id, Box::new(HookDriver::new()));

    port.open(&mut poller).expect("the driver starts");
    port.output(&mut poller, b"cmd").expect("the write");
    port.ready_input(&mut poller).expect("read readiness");
    port.ready_output(&mut poller).expect("write readiness");
    port.control(&mut poller, "verb", b"")
        .expect("the control op");
    port.timeout(&mut poller).expect("the timer");
    port.drive_async(&mut poller).expect("the async completion");
    port.notify_exit(&mut poller, crate::driver::PortExitReason::Closed)
        .expect("the exit notification");
    port.close(&mut poller).expect("the teardown");

    let completions = port.take_completions();
    assert_eq!(
        ids(&completions),
        vec![
            hook::START,
            hook::OUTPUT,
            hook::READY_INPUT,
            hook::READY_OUTPUT,
            hook::CONTROL,
            hook::TIMEOUT,
            hook::DRIVE_ASYNC,
            hook::PORT_EXIT,
            hook::STOP,
        ],
        "one completion per interaction point, in call order, none lost and none duplicated"
    );
    for completion in &completions {
        assert_eq!(
            completion.port, id,
            "the port stamps its own id on what its driver hands up"
        );
    }

    // Exactly once: draining is what removes them, so a second drain is empty even though the
    // driver's callbacks each delivered.
    assert!(
        port.take_completions().is_empty(),
        "a completion is reported exactly once"
    );
}

/// The timer path is the one the port task cannot report any other way: `Port::run_task`'s
/// `Timeout` arm discards `Driver::timeout`'s `Result`, so a `{error, timeout}` reply exists only
/// because it was staged as a completion.
#[test]
fn a_timer_driven_reply_escapes_through_the_completion_channel() {
    /// A driver whose timer answers the pending command with `{error, timeout}` — the C's
    /// `inet_reply_error_am(INETP(desc), am_timeout)` (`inet_drv.c:12480`).
    struct TimeoutReplyDriver;

    impl Driver for TimeoutReplyDriver {
        fn name(&self) -> &str {
            "completion-timeout-test"
        }

        fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
            let len = ctx.output().len();
            ctx.output().consume(len);
            Ok(len)
        }

        fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
            ctx.deliver_completion(CompletionEvent::reply_error(
                None,
                InetReason::Timeout,
            ))
        }

        fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }
    }

    let mut poller = poller();
    let id = PortId::new(11);
    let mut port = Port::new(id, Box::new(TimeoutReplyDriver));
    port.open(&mut poller).expect("the driver starts");

    // A due `TIMEOUT` task runs the driver's `timeout` through the real port-task path.
    assert!(port.schedule_timeout(std::time::Instant::now()));
    assert_eq!(port.next_task(), Some(crate::PortTaskKind::Timeout));
    let task = port.pop_ready_task(std::time::Instant::now()).expect("due");
    port.run_task(&mut poller, &task).expect("the timer runs");

    assert_eq!(
        port.take_completions(),
        vec![PortCompletion::new(
            id,
            CompletionEvent::reply_error(None, InetReason::Timeout)
        )],
        "the timer's `{{error, timeout}}` reply left the driver through the completion channel"
    );
}

// -----------------------------------------------------------------------------------------------
// `PortRegistry`: the drain the runtime calls
// -----------------------------------------------------------------------------------------------

/// A port-task round reports whatever its drivers staged — from the `Output` task, from the
/// `TIMEOUT` task and from the `driver_async` drive point — and a second drain is empty.
#[test]
fn a_port_task_round_reports_the_completions_it_staged() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("the port starts");
    // `start` staged one completion while the port was being opened, and the port armed the
    // driver's own deadline as its `TIMEOUT` task (`Port::refresh_timeout`).
    assert_eq!(ids(&registry.take_completions()), vec![hook::START]);
    assert!(
        registry.get(id).expect("live").has_pending_task(
            crate::PortTaskKind::Timeout
        ),
        "the driver's declared deadline is armed as the port's timer"
    );

    registry
        .port_command(&mut poller, id, b"cmd", PortCommandOptions::new())
        .expect("the command is queued");

    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(round.ran, 2, "the `Output` task and the `TIMEOUT` task ran");

    assert_eq!(
        ids(&registry.take_completions()),
        vec![hook::READY_OUTPUT, hook::TIMEOUT, hook::DRIVE_ASYNC],
        "the write task, the timer task and the async drive point each reported once"
    );
    assert!(
        registry.take_completions().is_empty(),
        "the round's completions were drained exactly once"
    );
}

/// `handle_events` runs the same scheduled tasks, so it stages the same completions — but it does
/// **not** drive `driver_async` (that drive point belongs to
/// [`PortRegistry::execute_port_tasks`]).
#[test]
fn handle_events_rounds_report_completions_too() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("the port starts");
    let _ = registry.take_completions();

    registry
        .port_command(&mut poller, id, b"cmd", PortCommandOptions::new())
        .expect("the command is queued");
    registry
        .handle_events(&mut poller, &[])
        .expect("the round runs");

    assert_eq!(
        ids(&registry.take_completions()),
        vec![hook::READY_OUTPUT, hook::TIMEOUT],
        "handle_events runs the write task and the timer, and reports what they staged"
    );
}

/// Two live ports are drained in **port-number order**, each one's own completions in delivery
/// order — the same determinism [`PortRegistry::take_line_notifications`] has.
#[test]
fn completions_are_drained_in_port_number_order() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let first = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("the first port");
    let second = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("the second port");
    assert!(
        first.number() < second.number(),
        "the id table hands out ascending numbers"
    );

    let drained = registry.take_completions();
    assert_eq!(
        drained,
        vec![
            PortCompletion::new(first, async_ok(hook::START)),
            PortCompletion::new(second, async_ok(hook::START)),
        ],
        "ascending port-number order, each port's own completions in delivery order"
    );
    assert!(registry.take_completions().is_empty());
}

/// A completion staged **before** a close — and the one the driver stages *during* its teardown
/// (the C's `async_error_am_all`, `inet_drv.c:2597-2608`) — both reach the owner exactly once
/// after the port is gone, and the port's **number** is still retired only by the `Closed`
/// report.
#[test]
fn a_completion_staged_before_a_close_is_reported_once_and_the_number_waits_for_the_closed_report() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("the port starts");
    // One staged by `start`, one by an inline write: both exist before the close.
    registry
        .output(&mut poller, id, b"x")
        .expect("the inline write");
    assert_eq!(ids(&registry.take_completions()), vec![hook::START, hook::OUTPUT]);

    registry.close(&mut poller, id).expect("the close");
    assert!(!registry.contains(id), "the closed port is gone");

    let harvested = registry.take_completions();
    assert_eq!(
        ids(&harvested),
        vec![hook::STOP],
        "the teardown's completion survives the close"
    );
    assert_eq!(harvested[0].port, id, "it still names the port it was for");
    assert!(
        registry.take_completions().is_empty(),
        "a harvested completion is reported exactly once"
    );

    // The completion did not touch the number's reservation: only the `Closed` report retires it.
    assert!(
        registry.port_table().contains(id),
        "an unreported close keeps its number reserved"
    );
    let next = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("a second port");
    assert_ne!(next, id, "the number is still reserved");
    let _ = registry.take_completions();

    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(
        round.notifications,
        vec![PortNotification::Closed { port: id }],
        "the close is reported as a `Closed` notification"
    );
    assert!(
        !registry.port_table().contains(id),
        "the `Closed` report retired the number"
    );
}

// -----------------------------------------------------------------------------------------------
// The event vocabulary
// -----------------------------------------------------------------------------------------------

/// The two `{inet_reply, …}` constructors, and the predicates that tell the families apart.
#[test]
fn the_reply_constructors_match_the_inet_drivers_two_shapes() {
    let tag = vec![131u8, 80, 0, 0, 0, 3];
    let ok = CompletionEvent::reply_ok(Some(tag.clone()));
    assert!(ok.is_inet_reply());
    assert!(!ok.is_inet_async());
    assert_eq!(
        ok,
        CompletionEvent::InetReply {
            caller_ref: Some(tag.clone()),
            reply: InetReply::Ok,
        },
        "the tag rides verbatim, as the bytes the caller sent"
    );

    let failed = CompletionEvent::reply_error(Some(tag.clone()), InetReason::Closed);
    assert_eq!(
        failed,
        CompletionEvent::InetReply {
            caller_ref: Some(tag),
            reply: InetReply::Error(InetReason::Closed),
        }
    );
    // A caller that sent no tag is `None`, not an empty tag: the C then sends a 3-tuple.
    assert_eq!(
        CompletionEvent::reply_ok(None),
        CompletionEvent::InetReply {
            caller_ref: None,
            reply: InetReply::Ok,
        }
    );
}

/// The reason vocabulary renders to the atom names the inet driver's `error_atom`/`am_closed`/
/// `am_timeout` produce, with the C's `errno_<N>` fallback for an errno the crate's errno table
/// does not name (`ERTS_UNKNOWN_ERRNO_FORMAT`, `erts/emulator/beam/erl_errno_str.c:47`).
#[test]
fn a_reason_renders_as_the_error_atom_it_must_become() {
    assert_eq!(InetReason::Closed.atom_name(), "closed");
    assert_eq!(InetReason::Timeout.atom_name(), "timeout");
    assert_eq!(InetReason::from_errno(libc::ENOENT).atom_name(), "enoent");
    assert_eq!(InetReason::from_errno(libc::EPIPE).atom_name(), "epipe");
    // An errno the crate's table does not name keeps the C's `errno_<N>` spelling rather than
    // being dropped: 4711 has no POSIX name at all.
    assert_eq!(InetReason::from_errno(4711).atom_name(), "errno_4711");
    // Both halves of the crate's errno set — the general POSIX/file codes and the socket ones —
    // render through the **one** table ([`crate::error::errno_name`]), so this accessor cannot
    // drift from the inet driver's `ctl_error` payloads. (`EPERM` is the case that used to slip
    // through a socket-only copy as `errno_1`.)
    for errno in [
        libc::EPERM,
        libc::EBADF,
        libc::EAGAIN,
        libc::ECONNRESET,
        libc::ENOTCONN,
        libc::ENOTSOCK,
        4711,
    ] {
        assert_eq!(
            InetReason::from_errno(errno).atom_name(),
            crate::error::errno_name(errno),
            "an errno reason is spelled by the crate's single errno table ({errno})"
        );
    }
    assert_eq!(InetReason::from_errno(libc::EPERM).atom_name(), "eperm");
    assert_eq!(
        InetReason::from_errno(libc::ECONNRESET).atom_name(),
        "econnreset"
    );
}

/// The four `{inet_async, …}` results are distinct and carry their payloads.
#[test]
fn the_async_results_carry_their_payloads() {
    let port = PortId::new(9);
    let results = [
        InetAsyncResult::Ok,
        InetAsyncResult::Data(ReadMessage::body_only(b"payload".to_vec())),
        InetAsyncResult::Port(port),
        InetAsyncResult::Error(InetReason::from_errno(libc::ECONNRESET)),
    ];
    for (i, a) in results.iter().enumerate() {
        for (j, b) in results.iter().enumerate() {
            assert_eq!(a == b, i == j, "{a:?} vs {b:?}");
        }
    }
    assert_eq!(
        results[1],
        InetAsyncResult::Data(ReadMessage::body_only(b"payload".to_vec())),
        "the received bytes ride the result"
    );
    assert_eq!(
        results[2],
        InetAsyncResult::Port(port),
        "the second port an accept created rides the result"
    );
}
