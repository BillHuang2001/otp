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

//! Integration tests for [`PortRegistry`]: the poller → port → driver path end to end.
//!
//! These use the built-in [`FdDriver`] over a real `pipe(2)`, driving the reactor exactly
//! as the runtime will: `Poller::poll` for readiness, then `PortRegistry::handle_events`
//! to dispatch it, then the port's input queue.

use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use beam_io::prelude::*;

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

fn fd_port() -> Box<dyn Driver> {
    Box::new(FdDriver::pipe().expect("a pipe"))
}

/// Polls and dispatches until `id` has buffered at least `want` bytes, returning them.
fn pump_until_input(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: beam_process::PortId,
    want: usize,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut received = Vec::new();
    while received.len() < want {
        assert!(
            Instant::now() < deadline,
            "the reactor never delivered {want} bytes"
        );
        let events = poller
            .poll(Some(Duration::from_millis(200)))
            .expect("poll succeeds");
        registry
            .handle_events(poller, &events)
            .expect("events dispatch");
        received.extend(registry.get_mut(id).expect("live").drain_input());
    }
    received
}

#[test]
fn output_and_input_round_trip_through_the_reactor() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let id = registry
        .open(&mut poller, fd_port())
        .expect("the port opens");
    assert!(registry.contains(id));
    assert_eq!(registry.len(), 1);
    let port = registry.get(id).expect("the port is live");
    assert_eq!(port.state(), PortState::Open);
    assert_eq!(port.driver_name(), "fd");
    assert_eq!(port.input_len(), 0);
    assert_eq!(port.output_len(), 0);

    let written = registry
        .output(&mut poller, id, b"hello")
        .expect("the driver accepts the output");
    assert_eq!(written, 5, "the whole payload is written to the pipe");

    assert_eq!(
        pump_until_input(&mut poller, &mut registry, id, 5),
        b"hello",
        "the bytes come back through the poller"
    );

    registry.close(&mut poller, id).expect("the port closes");
    assert!(registry.get(id).is_none());
    assert!(registry.is_empty());
    assert!(poller.is_empty(), "closing deregisters the port's fds");
}

#[test]
fn a_close_retires_the_id_only_once_it_is_reported() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let first = registry.open(&mut poller, fd_port()).expect("opens");
    let second = registry.open(&mut poller, fd_port()).expect("opens");
    assert_ne!(first, second, "each live port gets its own id");

    registry.close(&mut poller, first).expect("closes");
    assert!(!registry.contains(first));
    assert!(registry.contains(second));
    assert_eq!(registry.len(), 1);

    // The number of an unreported close stays reserved: the port-task round that reports the
    // close is what retires it, so a deferred `Closed` notification can never name a live port.
    let fresh = registry.open(&mut poller, fd_port()).expect("opens");
    assert_ne!(
        fresh, first,
        "an unreported close keeps its number out of circulation"
    );

    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert!(
        round
            .notifications
            .contains(&PortNotification::Closed { port: first }),
        "the round reports the close that retires the number"
    );

    // Retired with the report: the number is handed out again.
    let reused = registry.open(&mut poller, fd_port()).expect("opens");
    assert_eq!(reused, first, "the reported close retired its number");

    for id in [second, fresh, reused] {
        registry.close(&mut poller, id).expect("closes");
    }
    assert!(registry.is_empty());
}

#[test]
fn open_named_uses_the_built_in_driver_registry() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let drivers = DriverRegistry::with_builtins();

    let id = registry
        .open_named(&mut poller, &drivers, "fd")
        .expect("the fd driver is built in");
    assert_eq!(registry.get(id).expect("live").driver_name(), "fd");
    registry.close(&mut poller, id).expect("closes");

    // An unknown driver name is an argument fault, not a panic.
    assert!(matches!(
        registry.open_named(&mut poller, &drivers, "no-such-driver"),
        Err(beam_common::BeamError::BadArg)
    ));
    assert!(registry.is_empty());
}

#[test]
fn unknown_event_keys_are_ignored() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry.open(&mut poller, fd_port()).expect("opens");

    // Tokens are opaque; take one from a real registration. The *keys* are what the
    // reactor looks ports up by, so the forged events below reach no live port.
    let (_peer, probe) = UnixStream::pair().expect("socketpair");
    let token = poller
        .register(probe.as_raw_fd(), Interest::READABLE, WAKER_KEY)
        .expect("the probe registers");

    let forged = vec![
        Event::new(token, 4242, Ready::default()),
        Event::new(token, WAKER_KEY, Ready::default()),
    ];
    let outcome = registry
        .handle_events(&mut poller, &forged)
        .expect("dispatching is a no-op");
    assert_eq!(outcome.ran, 0, "events for unknown keys schedule no task");
    assert!(
        outcome.notifications.is_empty(),
        "events for unknown keys report nothing"
    );
    assert!(registry.contains(id), "the live port is untouched");
    assert_eq!(registry.len(), 1);
}
