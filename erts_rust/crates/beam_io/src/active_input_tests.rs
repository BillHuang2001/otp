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

//! The driver→owner **active-mode input** channel, pinned structurally.
//!
//! `beam_io` is term-free, so these tests assert *channel contents* — which port a delivery is
//! stamped with, in what order the deliveries come out, and that each is reported exactly once —
//! never the Erlang message the runtime builds from them (a `{tcp, S, Data}` / `{udp, S, IP,
//! Port, Data}` / `{tcp_passive, S}` / `{udp_passive, S}` tuple).
//!
//! * [`HookDriver`] delivers one identifiable [`ActiveInputEvent`] from **every** driver
//!   interaction point (`start`, `output`, `ready_input`, `ready_output`, `control`, `timeout`,
//!   `drive_async`, `port_exit`, `stop`), so one pass over the driver surface pins that the sink
//!   is installed everywhere. The payload byte is the hook's number, so a drain's contents and
//!   its order are both checkable.
//! * The `PortRegistry` tests pin the runtime-facing half: two ports are drained in
//!   **port-number** order, a second drain is empty, and a delivery staged before (or during) a
//!   [`PortRegistry::close`] still reaches the owner exactly once.
//! * One test is a **coherence** pin: [`PortNotification`] still has exactly its two variants and
//!   gains none of this channel's traffic, because `beam_rt` may not be edited and matches it
//!   exhaustively.
//!
//! Nothing here needs a descriptor, a child process, an `erl` oracle or a poll wait: the driver
//! registers no fd, so the ports are driven directly.

use std::collections::VecDeque;
use std::time::Instant;

use beam_common::Result;
use beam_process::PortId;

use crate::completion::InetReason;
use crate::driver::{Driver, DriverContext, PortArms, PortExitReason};
use crate::poller::Poller;
use crate::port::{Port, PortCommandOptions, DEFAULT_READINESS_RED_BUDGET};
use crate::port_failure::DriverFailure;
use crate::port_task::{ActiveInput, ActiveInputEvent, PortNotification, ReadMessage};
use crate::queue::ByteQueue;
use crate::registry::PortRegistry;

/// The hook numbers [`HookDriver`] uses as its payload byte, in the order
/// [`every_driver_interaction_point_can_deliver_active_input`] drives them.
mod hook {
    pub(super) const START: u8 = 1;
    pub(super) const OUTPUT: u8 = 2;
    pub(super) const READY_INPUT: u8 = 3;
    pub(super) const READY_OUTPUT: u8 = 4;
    pub(super) const CONTROL: u8 = 5;
    pub(super) const TIMEOUT: u8 = 6;
    pub(super) const DRIVE_ASYNC: u8 = 7;
    pub(super) const PORT_EXIT: u8 = 8;
    pub(super) const STOP: u8 = 9;
}

/// One `{tcp, S, Data}`-shaped delivery whose payload byte is `tag`, with a one-byte framing
/// header so the header/body split is visible in the drain too.
fn data(tag: u8) -> ActiveInputEvent {
    ActiveInputEvent::Data(ReadMessage {
        header: b"H".to_vec(),
        body: Some(vec![tag]),
    })
}

/// Stages one identifiable active-mode delivery, and is the port's only source of them: the
/// driver registers no descriptor, so nothing is ever polled and every callback below is reached
/// by a direct call.
fn stage(ctx: &mut DriverContext<'_>, tag: u8) -> Result<()> {
    ctx.deliver_active_input(data(tag))
}

/// Drains the port's output queue (so the port's write-accounting check passes) and then stages a
/// delivery — the order a real socket driver uses: the command goes out, the inbound message is
/// handed up.
fn write(ctx: &mut DriverContext<'_>, tag: u8) -> Result<usize> {
    let len = ctx.output().len();
    ctx.output().consume(len);
    stage(ctx, tag)?;
    Ok(len)
}

/// A driver that stages exactly one active-mode delivery from **every** interaction point it
/// implements.
///
/// It declares a **one-shot** driver deadline (`Driver::next_timeout`, consumed by its own
/// `timeout` callback) so the port arms its `TIMEOUT` task the way a real timer-using driver does,
/// and it reports pending asynchronous work so the round's `driver_async` drive point runs.
struct HookDriver {
    /// The deadline `Driver::next_timeout` declares, cleared by the `timeout` callback that
    /// consumes it.
    deadline: Option<Instant>,
}

impl HookDriver {
    /// A driver with a deadline that is already due, so the first round runs its timer.
    fn new() -> HookDriver {
        HookDriver {
            deadline: Some(Instant::now()),
        }
    }
}

impl Driver for HookDriver {
    fn name(&self) -> &str {
        "active-input-hook-test"
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
        self.deadline = None;
        stage(ctx, hook::TIMEOUT)
    }

    fn next_timeout(&self) -> Option<Instant> {
        self.deadline
    }

    fn drive_async(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        stage(ctx, hook::DRIVE_ASYNC)?;
        Ok(1)
    }

    fn has_pending_async(&self) -> bool {
        true
    }

    fn port_exit(&mut self, ctx: &mut DriverContext<'_>, _reason: PortExitReason) -> Result<()> {
        stage(ctx, hook::PORT_EXIT)
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        stage(ctx, hook::STOP)
    }
}

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// The `(port, payload byte)` pairs of a drained batch, in drain order; [`ActiveInputEvent::Passive`]
/// renders as the byte `0` (no hook stages one).
fn tags(deliveries: &[ActiveInput]) -> Vec<(PortId, u8)> {
    deliveries
        .iter()
        .map(|delivery| {
            let tag = match &delivery.event {
                ActiveInputEvent::Data(read) => read
                    .body
                    .as_deref()
                    .and_then(<[u8]>::first)
                    .copied()
                    .expect("every hook delivery carries a one-byte body"),
                ActiveInputEvent::Passive => 0,
                ActiveInputEvent::Error(_) => {
                    panic!("no hook stages an error event; the error path has its own tests")
                }
            };
            (delivery.port, tag)
        })
        .collect()
}

// -----------------------------------------------------------------------------------------------
// The channel primitive: `DriverContext::deliver_active_input`
// -----------------------------------------------------------------------------------------------

/// The `Error` reasons of a drained batch, rendered by their Erlang atom name, in drain order.
fn errors(deliveries: &[ActiveInput]) -> Vec<String> {
    deliveries
        .iter()
        .filter_map(|delivery| match &delivery.event {
            ActiveInputEvent::Error(reason) => Some(reason.atom_name()),
            ActiveInputEvent::Data(_) | ActiveInputEvent::Passive => None,
        })
        .collect()
}/// A context with no active-input sink is the typed `unsupported` naming the missing sink — never a
/// silent drop, because the owner may be blocked in `receive {tcp, S, _}`.
#[test]
fn deliver_active_input_without_a_sink_is_the_typed_unsupported() {
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
        .deliver_active_input(data(3))
        .expect_err("no sink is installed");
    assert!(
        error.to_string().contains("no active-input sink"),
        "the diagnostic names the missing sink: {error}"
    );
}

/// With a sink installed, the context **stamps its own port** — a driver cannot attribute a
/// delivery to another port — and each call is exactly one entry, in call order.
#[test]
fn the_context_stamps_each_delivery_with_its_own_port_and_never_merges_two() {
    let mut poller = poller();
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut sink = VecDeque::new();
    let mut ctx = DriverContext::new(
        PortId::new(7),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    )
    .with_active(&mut sink);

    let first = data(1);
    ctx.deliver_active_input(first.clone()).expect("a sink");
    ctx.deliver_active_input(data(2)).expect("a sink");

    assert_eq!(
        sink,
        VecDeque::from(vec![
            ActiveInput {
                port: PortId::new(7),
                event: first,
            },
            ActiveInput {
                port: PortId::new(7),
                event: data(2),
            },
        ]),
        "two calls are two deliveries in call order, each stamped with the context's port — \
         never merged into one and never re-stamped"
    );
}

/// An empty payload is still a real delivery: the oracle's `{tcp, S, <<>>}` /
/// `{udp, S, IP, Port, <<>>}` — which is why the channel carries a [`ReadMessage`] rather than
/// bytes, and why a consumer must not drop an empty one.
#[test]
fn an_empty_payload_is_still_a_delivery() {
    let mut poller = poller();
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut sink = VecDeque::new();
    let mut ctx = DriverContext::new(
        PortId::new(4),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    )
    .with_active(&mut sink);

    ctx.deliver_active_input(ActiveInputEvent::Data(ReadMessage::body_only(Vec::new())))
        .expect("a sink");
    assert_eq!(
        sink.len(),
        1,
        "a zero-length datagram is one delivery, not none"
    );
    assert_eq!(
        sink[0].event,
        ActiveInputEvent::Data(ReadMessage {
            header: Vec::new(),
            body: Some(Vec::new()),
        }),
        "an empty body is `Some(vec![])` (a present, empty payload), never `None`"
    );
}

/// A latched failure makes the delivery seam refuse — the same rule `deliver_read`/
/// `deliver_completion` follow (the Rust counterpart of the C's `-1` on a dying port), and the
/// refused delivery does **not** land in the sink.
#[test]
fn a_latched_failure_makes_deliver_active_input_refuse() {
    let mut poller = poller();
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut sink = VecDeque::new();
    let mut latch = Some(DriverFailure::posix(libc::EINVAL));
    let mut ctx = DriverContext::new(
        PortId::new(9),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    )
    .with_active(&mut sink)
    .with_failure(&mut latch);

    let error = ctx
        .deliver_active_input(data(5))
        .expect_err("the port is closing");
    assert!(
        error.to_string().contains("port is closing"),
        "the diagnostic says the port is on its way out: {error}"
    );
    assert!(
        sink.is_empty(),
        "a refused delivery is not silently staged after all"
    );
}

// -----------------------------------------------------------------------------------------------
// Every driver interaction point installs the sink
// -----------------------------------------------------------------------------------------------

/// One pass over the whole driver surface: `start`, `output`, `ready_input`, `ready_output`,
/// `control`, `timeout`, `drive_async`, `port_exit` and `stop` each stage a delivery, and the port
/// reports all nine **exactly once**, in the order the hooks ran, each stamped with the port's own
/// id.
///
/// This is the regression test for "the sink must be reachable from every driver interaction
/// point": a missing `.with_active(…)` on any one of those paths fails here.
#[test]
fn every_driver_interaction_point_can_deliver_active_input() {
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
    port.notify_exit(&mut poller, PortExitReason::Closed)
        .expect("the exit notification");
    port.close(&mut poller).expect("the teardown");

    let deliveries = port.take_active_inputs();
    assert_eq!(
        tags(&deliveries),
        vec![
            (id, hook::START),
            (id, hook::OUTPUT),
            (id, hook::READY_INPUT),
            (id, hook::READY_OUTPUT),
            (id, hook::CONTROL),
            (id, hook::TIMEOUT),
            (id, hook::DRIVE_ASYNC),
            (id, hook::PORT_EXIT),
            (id, hook::STOP),
        ],
        "one delivery per interaction point, in call order, none lost and none duplicated"
    );

    // Exactly once: draining is what removes them, so a second drain is empty even though the
    // driver's callbacks each delivered.
    assert!(
        port.take_active_inputs().is_empty(),
        "a delivery is reported exactly once"
    );
}

/// A `Passive` event crosses the whole path unchanged — the port stamps it like any other
/// delivery and adds no atom of its own (the runtime picks `{tcp_passive, S}` vs
/// `{udp_passive, S}` from the driving port's driver name).
#[test]
fn a_passive_event_survives_the_round_trip() {
    /// A driver whose read-readiness callback reports a spent `{active, N}` counter — the C's
    /// `packet_passive_message` (`inet_drv.c:4215`).
    struct PassiveDriver;

    impl Driver for PassiveDriver {
        fn name(&self) -> &str {
            "active-input-passive-test"
        }

        fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
            let len = ctx.output().len();
            ctx.output().consume(len);
            Ok(len)
        }

        fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
            ctx.deliver_active_input(ActiveInputEvent::Passive)
        }

        fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }
    }

    let mut poller = poller();
    let id = PortId::new(13);
    let mut port = Port::new(id, Box::new(PassiveDriver));
    port.open(&mut poller).expect("the driver starts");
    port.ready_input(&mut poller).expect("read readiness");

    assert_eq!(
        port.take_active_inputs(),
        vec![ActiveInput {
            port: id,
            event: ActiveInputEvent::Passive,
        }],
        "an active-mode counter transition is a delivery with no payload and no atom"
    );
    assert!(port.take_active_inputs().is_empty());
}

// -----------------------------------------------------------------------------------------------
// The third event: a read error
// -----------------------------------------------------------------------------------------------

/// A driver whose read-readiness callback stages the errors it was built with, in call order —
/// the C's `tcp_error_message` (`inet_drv.c:4079`) / `packet_error_message` (`:4245`) through
/// [`DriverContext::deliver_active_input`].
struct ErrorDriver {
    /// The reasons to stage, in order, from one `ready_input`.
    reasons: Vec<InetReason>,
}

impl Driver for ErrorDriver {
    fn name(&self) -> &str {
        "active-input-error-test"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let len = ctx.output().len();
        ctx.output().consume(len);
        Ok(len)
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        for reason in &self.reasons {
            ctx.deliver_active_input(ActiveInputEvent::Error(*reason))?;
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// A read error crosses the channel unchanged: one event, stamped with the port's own id, and a
/// second drain is empty. The reason is the term-free [`InetReason`] — the runtime picks
/// `{tcp_error, S, Reason}` vs `{udp_error, S, Reason}` from the port's driver name and renders the
/// atom with [`InetReason::atom_name`], so nothing here may invent one.
#[test]
fn an_error_event_survives_the_round_trip() {
    let mut poller = poller();
    let id = PortId::new(17);
    let mut port = Port::new(
        id,
        Box::new(ErrorDriver {
            reasons: vec![InetReason::Errno(libc::ECONNRESET)],
        }),
    );
    port.open(&mut poller).expect("the driver starts");
    port.ready_input(&mut poller).expect("read readiness");

    let delivered = port.take_active_inputs();
    assert_eq!(
        delivered,
        vec![ActiveInput {
            port: id,
            event: ActiveInputEvent::Error(InetReason::Errno(libc::ECONNRESET)),
        }],
        "one read error is one delivery, stamped with the context's own port"
    );
    assert_eq!(
        errors(&delivered),
        vec!["econnreset".to_string()],
        "the reason needs no atom of its own: the runtime renders it"
    );
    assert!(
        port.take_active_inputs().is_empty(),
        "a delivery is reported exactly once"
    );
}

/// Two errors staged in one callback are **two** deliveries in call order — never merged, and never
/// collapsed to one reason. The C's active arm sends the error and then the close
/// (`inet_drv.c:12880`-`:12881`); a driver that reports both reasons keeps them distinguishable.
#[test]
fn two_errors_in_one_callback_are_two_deliveries_in_call_order() {
    let mut poller = poller();
    let id = PortId::new(19);
    let mut port = Port::new(
        id,
        Box::new(ErrorDriver {
            reasons: vec![InetReason::Errno(libc::ECONNRESET), InetReason::Closed],
        }),
    );
    port.open(&mut poller).expect("the driver starts");
    port.ready_input(&mut poller).expect("read readiness");

    let delivered = port.take_active_inputs();
    assert_eq!(
        errors(&delivered),
        vec!["econnreset".to_string(), "closed".to_string()],
        "two staged errors are two deliveries, in call order: {delivered:?}"
    );
    assert!(
        delivered.iter().all(|input| input.port == id),
        "each delivery is stamped with the context's own port"
    );
}

// -----------------------------------------------------------------------------------------------
// `PortRegistry`: the drain the runtime calls
// -----------------------------------------------------------------------------------------------

/// Two live ports are drained in **port-number order**, each one's own deliveries in delivery
/// order — the same determinism [`PortRegistry::take_completions`] and
/// [`PortRegistry::take_line_notifications`] have — and a second drain is empty.
#[test]
fn active_inputs_are_drained_in_port_number_order_and_exactly_once() {
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

    assert_eq!(
        tags(&registry.take_active_inputs()),
        vec![(first, hook::START), (second, hook::START)],
        "ascending port-number order, each port's own deliveries in delivery order"
    );
    assert!(
        registry.take_active_inputs().is_empty(),
        "draining is what removes them, so a second drain is empty"
    );
}

/// A port-task round re-populates the channel through the ordinary task path — the `Output` task,
/// the `TIMEOUT` task and the `driver_async` drive point — and
/// [`PortRegistry::take_active_inputs`] is the accessor the runtime polls afterwards.
#[test]
fn a_port_task_round_reports_the_active_deliveries_it_staged() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("the port starts");
    // `start` staged one delivery while the port was being opened, and the port armed the
    // driver's own deadline as its `TIMEOUT` task (`Port::refresh_timeout`).
    assert_eq!(
        tags(&registry.take_active_inputs()),
        vec![(id, hook::START)]
    );

    registry
        .port_command(&mut poller, id, b"cmd", PortCommandOptions::new())
        .expect("the command is queued");
    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(round.ran, 2, "the `Output` task and the `TIMEOUT` task ran");
    assert_eq!(
        tags(&registry.take_active_inputs()),
        vec![
            (id, hook::READY_OUTPUT),
            (id, hook::TIMEOUT),
            (id, hook::DRIVE_ASYNC),
        ],
        "the write task, the timer task and the async drive point each reported once"
    );
    assert!(
        registry.take_active_inputs().is_empty(),
        "the round's deliveries were drained exactly once"
    );
}
/// A delivery staged **before** a close — and the one the driver stages *during* its teardown —
/// both reach the owner exactly once after the port is gone, and the port's **number** is still
/// retired only by the `Closed` report.
#[test]
fn a_delivery_staged_at_a_close_is_reported_once_and_the_number_waits_for_the_closed_report() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("the port starts");
    // One staged by `start`, one by an inline write: both exist before the close.
    registry
        .output(&mut poller, id, b"x")
        .expect("the inline write");
    assert_eq!(
        tags(&registry.take_active_inputs()),
        vec![(id, hook::START), (id, hook::OUTPUT)]
    );

    registry.close(&mut poller, id).expect("the close");
    assert!(!registry.contains(id), "the closed port is gone");

    let harvested = registry.take_active_inputs();
    assert_eq!(
        tags(&harvested),
        vec![(id, hook::STOP)],
        "the teardown's delivery survives the close"
    );
    assert_eq!(harvested[0].port, id, "it still names the port it was for");
    assert!(
        registry.take_active_inputs().is_empty(),
        "a harvested delivery is reported exactly once"
    );

    // The delivery did not touch the number's reservation: only the `Closed` report retires it.
    assert!(
        registry.port_table().contains(id),
        "an unreported close keeps its number reserved"
    );
    let next = registry
        .open(&mut poller, Box::new(HookDriver::new()))
        .expect("a second port");
    assert_ne!(next, id, "the number is still reserved");
    let _ = registry.take_active_inputs();

    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(
        round.notifications,
        vec![PortNotification::Closed { port: id }],
        "the close is still reported as a `Closed` notification — the active channel adds nothing"
    );
    assert!(
        !registry.port_table().contains(id),
        "the `Closed` report retired the number"
    );
}

// -----------------------------------------------------------------------------------------------
// Coherence: the two frozen neighbour shapes must not change
// -----------------------------------------------------------------------------------------------

/// [`PortNotification`] keeps **exactly** its two variants: an active-mode delivery rides the
/// separate [`ActiveInput`] channel, so a consumer that matches the notification exhaustively
/// (every `beam_rt` round driver) keeps compiling and never sees a `{tcp, …}` message on the
/// `{Port, {data, …}}` path.
///
/// The exhaustiveness *is* the assertion: this `match` has two arms and no wildcard, so a third
/// variant would not compile.
#[test]
fn port_notification_still_has_exactly_two_variants() {
    let port = PortId::new(3);
    let notifications = [
        PortNotification::Data {
            port,
            read: ReadMessage::body_only(vec![7]),
        },
        PortNotification::Closed { port },
    ];

    let rendered: Vec<&str> = notifications
        .iter()
        .map(|notification| match notification {
            PortNotification::Data { port: p, read } => {
                assert_eq!(*p, port);
                assert_eq!(read.flattened(), vec![7]);
                "data"
            }
            PortNotification::Closed { port: p } => {
                assert_eq!(*p, port);
                "closed"
            }
        })
        .collect();
    assert_eq!(rendered, vec!["data", "closed"]);
}
