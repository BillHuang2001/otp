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

//! Path-included tests for the `driver_output*` hook family of [`super`] (`abi::host`): the
//! **one read message per call** contract, its header/body split, and the end-to-end delivery to
//! the port owner.
//!
//! Split out of `host_tests.rs` (a sibling `#[path]` child of `host`, included from `host.rs`) so
//! neither file grows past the workspace's ~1000-line convention: this file owns the output path,
//! that one owns the host's own plumbing (select/async/queue/registry slots). Both are children of
//! `host`, so `super::*` still reaches the module's private items and the shared fixtures are
//! `pub(super)` in `host_tests`.
//!
//! The pinned ground truth is `ReadMessage`'s oracle table
//! (`../../../erts/emulator/beam/io.c:3363-3442`, `deliver_read_message`): one
//! `{Port, {data, _}}` per `driver_output*` **call**, two calls in one callback are two messages, and
//! the C's `buf == NULL` (a header-only proper list) is a different message from a present-but-empty
//! body (`[H…|<<>>]`).

use super::tests::{new_context_parts, new_host};
use super::AbiHost;
use beam_abi::driver::host::DriverHost;
use beam_common::Result;
use beam_process::PortId;
use std::collections::VecDeque;

use crate::driver::Driver;
use crate::poller::Poller;
use crate::port::Port;
use crate::port_task::{PortNotification, ReadMessage};
use crate::DriverContext;

/// `driver_output` is the C's `driver_output2(ix, NULL, 0, buf, len)` (`io.c:6405-6415`), so every
/// call is **one read message of its own**: its bytes are the read's *body* (never a header), and
/// nothing is banked on the port's input accumulator — which is precisely what used to coalesce two
/// calls made in one callback into a single `{data, _}`.
///
/// A zero-length call is still a call, so it stages a read with a **present** empty body
/// (`Some(vec![])` — the C's non-NULL `buf` of length 0): the oracle renders it `{data, []}` in
/// `[list]` mode and `{data, <<>>}` in `[binary]` mode, i.e. a real message, unlike the
/// `buf == NULL` header-only shape [`DriverHost::output2`] carries as `body: None`.
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
                HostCall::Output2(header, body) => {
                    DriverHost::output2(&mut self.host, core::ptr::null_mut(), header, *body)
                }
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
