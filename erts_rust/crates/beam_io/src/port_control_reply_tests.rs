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
//! The **control-reply shape surface** (`Port::control_reply_shape`,
//! `PortRegistry::control_reply_shape`/`port_control_reply`) over a plain-Rust driver.
//!
//! The C answers a `control` reply from two independent facts, and the reply pointer is tested
//! **first**: `write_port_control_result` opens with `if (!resp_bufp) return NIL;`
//! (`erts/emulator/beam/io.c:4134`), so a `NULL` `*rbuf` is the empty **list** `[]` for **every**
//! flag word — `PORT_CONTROL_FLAG_BINARY` does not change it — while only a zero-length *buffer*
//! under that flag is `<<>>` (`io.c:4136-4162`). Both reach a caller as the same empty `Vec<u8>`,
//! so the shape has to travel beside the bytes; that is what these tests pin:
//!
//! * [`Port::control_reply_shape`] forwards the driver's answer, and a driver with no opinion
//!   inherits [`ControlReplyShape::Buffer`] — the honest answer for every built-in driver, which
//!   returns a real `Vec`;
//! * [`PortRegistry::port_control_reply`] answers bytes **and** shape, forwards a `Null` a driver
//!   reports, answers an empty `Buffer` for the reserved `close` op (no driver ran, so the oracle's
//!   `[]`), and is `badarg` for an unknown id;
//! * [`PortRegistry::port_control`] keeps its exact `Result<Vec<u8>>` signature and yields the same
//!   bytes — the shape is additive, never a signature change.
//!
//! The C-driver half of the same matrix (a real `set_port_control_flags` through a loaded `.so`,
//! plus the live-`erl` oracle row) is `tests/abi_driver.rs`; this suite needs no `cc`, no `erl` and
//! no descriptor.

use std::cell::Cell;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_process::PortId;

use crate::driver::{ControlReplyShape, Driver, DriverContext};
use crate::poller::Poller;
use crate::port::{Port, PortControlReply, PortState};
use crate::registry::PortRegistry;

/// The reply a [`ShapeDriver`] describes, plus the bytes it answers.
#[derive(Clone, Copy)]
struct Answer {
    bytes: &'static [u8],
    shape: ControlReplyShape,
}

/// A driver whose `control` answers one fixed [`Answer`] and reports its shape — the plain-Rust
/// stand-in for a C driver's `*rbuf` bookkeeping.
struct ShapeDriver {
    answer: Rc<Cell<Answer>>,
}

impl ShapeDriver {
    fn new(answer: Answer) -> (ShapeDriver, Rc<Cell<Answer>>) {
        let answer = Rc::new(Cell::new(answer));
        (
            ShapeDriver {
                answer: Rc::clone(&answer),
            },
            answer,
        )
    }
}

impl Driver for ShapeDriver {
    fn name(&self) -> &str {
        "shape-test"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let len = ctx.output().front().len();
        ctx.output().consume(len);
        Ok(len)
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn control(
        &mut self,
        _ctx: &mut DriverContext<'_>,
        _command: &str,
        _arg: &[u8],
    ) -> Result<Vec<u8>> {
        Ok(self.answer.get().bytes.to_vec())
    }

    fn control_reply_shape(&self) -> ControlReplyShape {
        self.answer.get().shape
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// A poller plus an open port over `driver`, and the driver's `answer` handle.
fn open_port(driver: ShapeDriver) -> (Poller, Port) {
    let mut poller = Poller::new().expect("a poll set");
    let mut port = Port::new(PortId::new(1), Box::new(driver));
    port.open(&mut poller).expect("the driver starts");
    (poller, port)
}

/// A registry holding one open port over `driver` (the built-ins register nothing here).
fn open_registry(driver: ShapeDriver) -> (Poller, PortRegistry, PortId) {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the driver opens as a port");
    (poller, registry, id)
}

/// `Port::control_reply_shape` forwards the driver's answer, and starts at the `Buffer` default.
#[test]
fn a_port_forwards_the_shape_its_driver_reports() {
    let (driver, answer) = ShapeDriver::new(Answer {
        bytes: b"",
        shape: ControlReplyShape::Buffer,
    });
    let (mut poller, mut port) = open_port(driver);
    assert_eq!(
        port.control_reply_shape(),
        ControlReplyShape::Buffer,
        "a driver with no opinion inherits the `Buffer` default (every built-in returns a real Vec)"
    );

    port.control(&mut poller, "verb", &[])
        .expect("the driver answers");
    assert_eq!(port.control_reply_shape(), ControlReplyShape::Buffer);

    // The driver flips to the `NULL` shape — the row a runtime must render as `[]` even under
    // `PORT_CONTROL_FLAG_BINARY`.
    answer.set(Answer {
        bytes: b"",
        shape: ControlReplyShape::Null,
    });
    assert_eq!(
        port.control_reply_shape(),
        ControlReplyShape::Null,
        "the port reads the shape back from its driver after the control call"
    );
    port.control(&mut poller, "verb", &[])
        .expect("the driver answers");
    assert_eq!(
        port.control(&mut poller, "verb", &[]).expect("still answering"),
        Vec::<u8>::new(),
        "a NULL reply carries no bytes at all"
    );
    assert_eq!(port.control_reply_shape(), ControlReplyShape::Null);
    assert_eq!(port.state(), PortState::Open);
    port.close(&mut poller).expect("the port closes");
}

/// `port_control_reply` answers both halves, and `port_control` keeps its bytes-only signature.
#[test]
fn the_registry_answers_the_bytes_and_the_shape() {
    let (driver, answer) = ShapeDriver::new(Answer {
        bytes: b"abc",
        shape: ControlReplyShape::Buffer,
    });
    let (mut poller, mut registry, id) = open_registry(driver);

    let reply: PortControlReply = registry
        .port_control_reply(&mut poller, id, "verb", &[])
        .expect("the driver answers");
    assert_eq!(reply.bytes, b"abc".to_vec());
    assert_eq!(reply.shape, ControlReplyShape::Buffer);

    // The bytes-only entry point is the same reply, unchanged.
    assert_eq!(
        registry
            .port_control(&mut poller, id, "verb", &[])
            .expect("the bytes-only form answers"),
        b"abc".to_vec()
    );
    assert_eq!(
        registry.control_reply_shape(id).expect("the port is live"),
        ControlReplyShape::Buffer
    );

    // The `NULL` row: no bytes, and the shape is what tells it apart from an empty buffer.
    answer.set(Answer {
        bytes: b"",
        shape: ControlReplyShape::Null,
    });
    let reply = registry
        .port_control_reply(&mut poller, id, "verb", &[])
        .expect("the driver answers the NULL shape");
    assert!(reply.bytes.is_empty());
    assert_eq!(
        reply.shape,
        ControlReplyShape::Null,
        "`[]` for every flag word — never `<<>>` (`io.c:4134`)"
    );
    assert_eq!(
        registry
            .port_control(&mut poller, id, "verb", &[])
            .expect("the bytes-only form answers"),
        Vec::<u8>::new()
    );
    assert_eq!(
        registry.control_reply_shape(id).expect("the port is live"),
        ControlReplyShape::Null
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// The reserved `close` op runs no driver, so there is no reply pointer: an empty `Buffer` (`[]`),
/// and the port is gone afterwards.
#[test]
fn the_reserved_close_op_answers_an_empty_buffer_reply() {
    let (driver, _answer) = ShapeDriver::new(Answer {
        bytes: b"unused",
        shape: ControlReplyShape::Null,
    });
    let (mut poller, mut registry, id) = open_registry(driver);

    let reply = registry
        .port_control_reply(&mut poller, id, "close", &[])
        .expect("the reserved close op answers");
    assert!(
        reply.bytes.is_empty(),
        "a close reply carries no bytes (the C closes the port and answers NIL)"
    );
    assert_eq!(
        reply.shape,
        ControlReplyShape::Buffer,
        "no driver ran, so no `*rbuf` existed — and the oracle answers `[]` for a close, never `<<>>`"
    );

    // Both entry points are `badarg` for the now-dead port.
    assert_eq!(
        registry
            .port_control_reply(&mut poller, id, "verb", &[])
            .expect_err("a closed port has no reply")
            .as_atom(),
        Some("badarg")
    );
    assert_eq!(
        registry
            .control_reply_shape(id)
            .expect_err("a closed port has no shape")
            .as_atom(),
        Some("badarg")
    );
}

/// An unknown id is `badarg` for both new entry points (as for every other registry seam).
#[test]
fn an_unknown_port_is_badarg() {
    let (driver, _answer) = ShapeDriver::new(Answer {
        bytes: b"",
        shape: ControlReplyShape::Buffer,
    });
    let (mut poller, mut registry, id) = open_registry(driver);
    let unknown = PortId::new(999);

    let err: BeamError = registry
        .port_control_reply(&mut poller, unknown, "verb", &[])
        .expect_err("an unknown port cannot answer");
    assert_eq!(err.as_atom(), Some("badarg"));
    assert_eq!(
        registry
            .control_reply_shape(unknown)
            .expect_err("an unknown port has no shape")
            .as_atom(),
        Some("badarg")
    );

    registry.close(&mut poller, id).expect("the port closes");
}
