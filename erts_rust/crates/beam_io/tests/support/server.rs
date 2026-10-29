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

#![allow(dead_code)]
//! The shared **TCP server harness** for `tests/inet_server.rs`.
//!
//! `tests/inet_accept.rs` pins the accept *chain* (completion shapes only), `tests/active_mode.rs`
//! pins framed/active delivery on a `from_socket` connection and
//! `src/drivers/inet/accept_opts_tests.rs` pins option inheritance at the driver level. What none
//! of them does is **chain an accepted socket into a served request/response** the way a
//! Cowboy/ranch listener is driven. This module is the plumbing for that: one [`ServerRig`] holds
//! the three things `beam_rt`'s main loop holds (a [`Poller`], a [`PortRegistry`] and the
//! [`DriverRegistry`]), every control verb goes through [`PortRegistry::port_control`] with the
//! **numeric** `INET_REQ_*` opcode `prim_inet` sends (`Op + 0x03f1a300`), and every wait is a
//! **condition** pumped under a hang guard — never a fixed sleep used as a correctness device.
//!
//! # The discipline (`beam_rt`'s round recipe)
//!
//! 1. `Poller::poll` → [`PortRegistry::handle_events`] (which schedules **and** runs the
//!    readiness tasks) → [`PortRegistry::execute_port_tasks`] (the round's `drive_async` and
//!    `TIMEOUT` run point).
//! 2. Drain **all four** owner channels after every round — `take_active_inputs`,
//!    `take_completions`, the round's notifications and `take_failures`: each drain is
//!    destructive, so an undrained event is lost.
//!
//! [`ServerRig::wait_until`] accumulates those drains into one [`Drain`] and returns as soon as
//! the caller's predicate holds, so a delivery that arrives early (in the same round as another
//! port's completion) is never dropped on the floor.
//!
//! # The oracle harness
//!
//! [`erl_reply_dump`] runs a real `tcp_inet` port under the system runtime and dumps its
//! `port_control/3` replies as comma-separated bytes, one `|`-separated field per reply (the
//! `src/drivers/inet/opt_oracle.rs` idiom, which an integration test cannot reach). [`run_erl`]
//! runs a bespoke expression and hands back its stdout. Both **skip loudly** (`SKIP:` banner via
//! [`super::skip`]) when `erl` is unavailable or exceeds [`ERL_TIMEOUT`], so no oracle row can
//! ever fail for want of a tool; a non-zero exit *is* an assertion failure — that is the oracle
//! speaking.

use std::net::TcpStream;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::drivers::inet::{
    INET_AF_INET, INET_AF_LOOPBACK, INET_INFINITY, INET_LOPT_ACTIVE, INET_MULTI,
    INET_OPT_KEEPALIVE, INET_OPT_RCVBUF, INET_OPT_SNDBUF, INET_PROTO_DEFAULT, INET_REP_ERROR,
    INET_REP_OK, INET_TYPE_STREAM, TCP_OPT_NODELAY, TCP_SHUT_WR,
};
use beam_io::prelude::*;
use beam_process::PortId;

use super::oracle::on_path;
use super::skip;

/// The upper bound a condition wait may take before the test fails — a hang guard, never a
/// wall-clock budget tuned for an idle host.
pub const WAIT: Duration = Duration::from_secs(10);

/// How long one round blocks in the reactor when nothing is ready.
pub const POLL_SLICE: Duration = Duration::from_millis(20);

/// How long a "nothing more arrives" drain keeps pumping the reactor. Long enough to cover
/// several poll slices, short enough that the negative rows stay cheap.
pub const QUIET_BUDGET: Duration = Duration::from_millis(200);

/// Wall-clock budget for one `erl` oracle session before the affected check is skipped.
pub const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`): what
/// `erlang:port_control/3` adds to every opcode, and what the drivers subtract again.
pub const CONTROL_MAGIC: u32 = 0x03f1_a300;

// The C request numbers this suite drives (`inet_drv.c:805`-`:847`).
pub const INET_REQ_OPEN: u32 = 1;
pub const INET_REQ_CLOSE: u32 = 2;
pub const INET_REQ_PEER: u32 = 4;
pub const INET_REQ_NAME: u32 = 5;
pub const INET_REQ_BIND: u32 = 6;
pub const INET_REQ_SETOPTS: u32 = 7;
pub const INET_REQ_GETOPTS: u32 = 8;
pub const INET_REQ_GETFD: u32 = 14;
pub const INET_REQ_GETSTATUS: u32 = 16;
pub const INET_REQ_ACCEPT: u32 = 26;
pub const INET_REQ_LISTEN: u32 = 27;
pub const TCP_REQ_RECV: u32 = 42;
pub const TCP_REQ_SHUTDOWN: u32 = 44;

// The option codes the driver does not re-export (`src/drivers/inet/opt.rs`).
/// `{reuseaddr, Bool}` (`INET_OPT_REUSEADDR`, `inet_drv.c:869`).
pub const INET_OPT_REUSEADDR: u8 = 0;
/// `{recbuf, N}` (`INET_OPT_RCVBUF`, `inet_drv.c:875`).
pub const INET_OPT_RCVBUF_CODE: u8 = INET_OPT_RCVBUF;
/// `{sndbuf, N}` (`INET_OPT_SNDBUF`, `inet_drv.c:874`).
pub const INET_OPT_SNDBUF_CODE: u8 = INET_OPT_SNDBUF;
/// `{nodelay, Bool}` (`TCP_OPT_NODELAY`, `inet_drv.c:879`).
pub const TCP_OPT_NODELAY_CODE: u8 = TCP_OPT_NODELAY;
/// `{keepalive, Bool}` (`INET_OPT_KEEPALIVE`, `inet_drv.c:870`).
pub const INET_OPT_KEEPALIVE_CODE: u8 = INET_OPT_KEEPALIVE;
/// `{buffer, N}` (`INET_LOPT_BUFFER`, `inet_drv.c:885`).
pub const INET_LOPT_BUFFER: u8 = 20;
/// `{mode, list | binary}` (`INET_LOPT_MODE`, `inet_drv.c:899`).
pub const INET_LOPT_MODE: u8 = 24;
/// `{mode, binary}`'s value (`INET_MODE_BINARY`, `inet_drv.c:779`).
pub const INET_MODE_BINARY: i32 = 1;

/// A listener with no accept queued: `INET_STATE_LISTENING` = `OPEN | INET_F_LISTEN` = `1 | 8`.
/// Measured on the oracle as `[1,0,0,0,9]`.
pub const LISTENING_STATUS: u32 = 9;
/// One queued accept: `9 | INET_F_ACC` (`0x20`) = `0x29`. Measured on the oracle as `41`.
pub const ONE_ACCEPT_STATUS: u32 = 41;
/// Two or more queued accepts: `9 | 0x20 | INET_F_MULTI_CLIENT` (`0x100`) = `0x129`. Measured on
/// the oracle as `297`.
pub const MULTI_ACCEPT_STATUS: u32 = 297;
/// `INET_STATE_CONNECTED` = `OPEN | INET_F_ACTIVE` = `1 | 4`. Measured on the oracle as
/// `[1,0,0,0,5]` for an adopted (connected) descriptor.
pub const CONNECTED_STATUS: u32 = 5;

/// A golden `term_to_binary(Mref, [local])` image — the 26-byte caller tag `prim_inet:send/4`
/// prefixes onto a command (`erts/preloaded/src/prim_inet.erl:575`-`:620`). The crate carries a
/// tag verbatim and never decodes it, so only the shape matters (`131` = `VERSION_MAGIC`,
/// `121` = `NEWER_REFERENCE_EXT`); `tests/inet_async.rs` proves an oracle-generated tag crosses
/// the same way.
pub const GOLDEN_TAG: [u8; 26] = [
    131, 121, 254, 153, 216, 200, 90, 0, 3, 106, 0, 0, 0, 0, 0, 0, 11, 218, 194, 188, 0, 11, 244,
    41, 135, 150,
];

// ---------------------------------------------------------------------------
// Wire vocabulary
// ---------------------------------------------------------------------------

/// The packed `INET_REQ_OPEN` argument for a stream socket: `[AF_INET, SOCK_STREAM, default]`.
pub fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
}

/// The packed `bind` argument naming `127.0.0.1:port` (the C's three-byte form; a zero port asks
/// the kernel for an ephemeral one).
pub fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_LOOPBACK, hi, lo]
}

/// The `Timeout(4)` operand of `INET_REQ_ACCEPT`(26) (`inet_drv.c:12160`).
pub fn accept_operand(timeout_ms: i32) -> Vec<u8> {
    timeout_ms.to_be_bytes().to_vec()
}

/// The `Timeout(4), Length(4)` operand of `TCP_REQ_RECV`(42) (`inet_drv.c:12293`).
pub fn recv_operand(timeout_ms: i32, length: u32) -> Vec<u8> {
    let mut arg = timeout_ms.to_be_bytes().to_vec();
    arg.extend_from_slice(&length.to_be_bytes());
    arg
}

/// One five-byte `[opt, value_be32]` `setopts` record.
pub fn record(code: u8, value: i32) -> Vec<u8> {
    let mut bytes = vec![code];
    bytes.extend_from_slice(&value.to_be_bytes());
    bytes
}

/// The **`{active, N}`** record: the option code, the value [`INET_MULTI`], then the trailing
/// big-endian 16-bit counter — `inet:setopts(S, [{active, N}])`'s wire form (`inet_drv.c:6977`).
pub fn active_multi(n: i16) -> Vec<u8> {
    let mut bytes = record(INET_LOPT_ACTIVE, INET_MULTI);
    bytes.extend_from_slice(&n.to_be_bytes());
    bytes
}

/// Concatenates option records into one `setopts` payload.
pub fn records(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flatten().copied().collect()
}

/// The C's `ctl_ok()` reply: `[INET_REP_OK]`.
pub fn ctl_ok() -> Vec<u8> {
    vec![INET_REP_OK]
}

/// The C's `ctl_xerror(name)` reply: the `[INET_REP_ERROR]` tag followed by the lowercase name
/// (`inet_drv.c:2307`).
pub fn ctl_xerror(name: &str) -> Vec<u8> {
    let mut reply = vec![INET_REP_ERROR];
    reply.extend_from_slice(name.as_bytes());
    reply
}

/// A `prim_inet:send/4`-shaped command: `[n:be16][caller tag][payload]`.
pub fn command(payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(GOLDEN_TAG.len() as u16).to_be_bytes());
    bytes.extend_from_slice(&GOLDEN_TAG);
    bytes.extend_from_slice(payload);
    bytes
}

/// The descriptor-state bits a `GETSTATUS` reply carries (`[1] ++ be32(state)`).
pub fn status_of_reply(reply: &[u8]) -> u32 {
    let [tag, a, b, c, d] = reply[..] else {
        panic!("a getstatus reply is `[1] ++ be32(state)`, got {reply:?}");
    };
    assert_eq!(tag, INET_REP_OK, "the getstatus result tag, got {reply:?}");
    u32::from_be_bytes([a, b, c, d])
}

/// The `(address, port)` a packed address reply names: `[1, AF_INET, port_hi, port_lo, a, b, c, d]`.
pub fn split_address_reply(reply: &[u8]) -> (u16, [u8; 4]) {
    let [tag, family, hi, lo, a, b, c, d] = reply[..] else {
        panic!("a packed address reply is `[1, AF, port_hi, port_lo, a, b, c, d]`, got {reply:?}");
    };
    assert_eq!(tag, INET_REP_OK, "the address result tag, got {reply:?}");
    assert_eq!(family, INET_AF_INET, "the packed family tag, got {reply:?}");
    (u16::from_be_bytes([hi, lo]), [a, b, c, d])
}

/// The descriptor a `GETFD` reply carries (`[1] ++ fd_be32`).
pub fn fd_of_reply(reply: &[u8]) -> i32 {
    let [tag, a, b, c, d] = reply[..] else {
        panic!("a getfd reply is `[1] ++ fd_be32`, got {reply:?}");
    };
    assert_eq!(tag, INET_REP_OK, "the getfd result tag, got {reply:?}");
    i32::from_be_bytes([a, b, c, d])
}

/// How many descriptors this process holds, or `None` where `/proc` is unavailable (the caller
/// then skips loudly instead of failing).
pub fn open_fd_count() -> Option<usize> {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|entries| entries.count())
}

// ---------------------------------------------------------------------------
// The rig: a poller, a registry, the driver registry — `beam_rt`'s main loop
// ---------------------------------------------------------------------------

/// What one reactor round observed.
#[derive(Debug)]
pub struct Round {
    /// How many poller events the round was handed — the non-vacuity witness that the reactor
    /// really ran.
    pub events: usize,
    pub active: Vec<ActiveInput>,
    pub completions: Vec<PortCompletion>,
    pub notifications: Vec<PortNotification>,
    pub failures: Vec<PortFailure>,
}

/// Everything the owner channels of one or more rounds reported, accumulated in arrival order.
///
/// A wait returns this instead of the last round's outcome so a delivery that shared its round
/// with something else is never lost.
#[derive(Debug, Default)]
pub struct Drain {
    /// How many rounds were pumped to satisfy the condition.
    pub rounds: usize,
    /// How many poller events those rounds carried in total.
    pub events: usize,
    pub active: Vec<ActiveInput>,
    pub completions: Vec<PortCompletion>,
    pub notifications: Vec<PortNotification>,
    pub failures: Vec<PortFailure>,
}

impl Drain {
    /// Folds one round's drained channels in.
    fn absorb(&mut self, round: Round) {
        self.rounds += 1;
        self.events += round.events;
        self.active.extend(round.active);
        self.completions.extend(round.completions);
        self.notifications.extend(round.notifications);
        self.failures.extend(round.failures);
    }

    /// Every `{active, …}` **data** delivery of the drain, in order.
    ///
    /// A `Data` drain never carries a read error: an [`ActiveInputEvent::Error`] is the
    /// `{tcp_error, S, Reason}` half, which no row here arms.
    pub fn data(&self) -> Vec<&ReadMessage> {
        let mut reads = Vec::new();
        for input in &self.active {
            match &input.event {
                ActiveInputEvent::Data(read) => reads.push(read),
                ActiveInputEvent::Passive => {}
                ActiveInputEvent::Error(reason) => panic!(
                    "a data drain carries no read error (`{}`): {:?}",
                    reason.atom_name(),
                    self.active
                ),
            }
        }
        reads
    }

    /// The `flattened()` body of every `{active, …}` data delivery, in order.
    pub fn bodies(&self) -> Vec<Vec<u8>> {
        self.data().iter().map(|read| read.flattened()).collect()
    }

    /// How many `{active, N}` grants the drain reported as spent ([`ActiveInputEvent::Passive`]).
    pub fn passive(&self) -> usize {
        self.active
            .iter()
            .filter(|input| input.event == ActiveInputEvent::Passive)
            .count()
    }

    /// The first completion the drain reported for `port`.
    pub fn completion_for(&self, port: PortId) -> Option<&PortCompletion> {
        self.completions.iter().find(|done| done.port == port)
    }

    /// The `(id, fd)` of the drain's single accept completion.
    pub fn accepted(&self) -> Option<(u16, i32)> {
        self.completions.iter().find_map(|done| match done.event {
            CompletionEvent::InetAsync {
                id,
                result: InetAsyncResult::Accepted { fd },
            } => Some((id, fd)),
            _ => None,
        })
    }

    /// Every `InetAsyncResult::Data` completion the drain reported for `port`, in order.
    pub fn data_for(&self, port: PortId) -> Vec<&ReadMessage> {
        let mut reads = Vec::new();
        for done in &self.completions {
            if done.port != port {
                continue;
            }
            if let CompletionEvent::InetAsync {
                result: InetAsyncResult::Data(read),
                ..
            } = &done.event
            {
                reads.push(read);
            }
        }
        reads
    }

    /// The `{inet_reply, …}` completion the drain reported for `port`, panicking on any other
    /// shape.
    pub fn reply_for(&self, port: PortId) -> Option<&InetReply> {
        self.completions.iter().find_map(|done| match &done.event {
            CompletionEvent::InetReply { reply, .. } if done.port == port => Some(reply),
            _ => None,
        })
    }
}

/// A live poll set, a port registry and the built-in driver registry — the three things
/// `beam_rt`'s main loop holds.
pub struct ServerRig {
    pub poller: Poller,
    pub ports: PortRegistry,
    pub drivers: DriverRegistry,
    /// How many rounds this rig has run — reported in failure messages, never asserted.
    pub rounds: usize,
}

impl ServerRig {
    pub fn new() -> ServerRig {
        ServerRig {
            poller: Poller::new().expect("a poll set"),
            ports: PortRegistry::new(),
            drivers: DriverRegistry::with_builtins(),
            rounds: 0,
        }
    }

    // -- ports ---------------------------------------------------------------------------------

    /// Opens a `"tcp_inet"` port through the built-in registry — the way the runtime resolves
    /// `open_port({spawn_driver, "tcp_inet"}, …)`. Every port of a rig lives in the **same**
    /// registry: port numbers are that registry's single id space and the poller keys are those
    /// numbers.
    pub fn open_tcp(&mut self) -> PortId {
        self.ports
            .open_named(&mut self.poller, &self.drivers, "tcp_inet")
            .expect("a tcp_inet port opens")
    }

    /// One control call through the **numeric** opcode route (`Op + CONTROL_MAGIC`, the decimal
    /// command string `erlang:port_control/3` sends).
    pub fn op(&mut self, id: PortId, code: u32, arg: &[u8]) -> Vec<u8> {
        let op = (u64::from(code) + u64::from(CONTROL_MAGIC)).to_string();
        self.ports
            .port_control(&mut self.poller, id, &op, arg)
            .unwrap_or_else(|err| panic!("the {code} control op answered: {err:?}"))
    }

    /// `INET_REQ_SETOPTS`(7), asserting the walk accepted the records.
    pub fn setopts(&mut self, id: PortId, parts: &[Vec<u8>]) -> Vec<u8> {
        let reply = self.op(id, INET_REQ_SETOPTS, &records(parts));
        assert_eq!(reply, ctl_ok(), "the option walk accepted every record");
        reply
    }

    /// `INET_REQ_GETOPTS`(8) over `codes`, in request order.
    pub fn getopts(&mut self, id: PortId, codes: &[u8]) -> Vec<u8> {
        self.op(id, INET_REQ_GETOPTS, codes)
    }

    /// `INET_REQ_GETSTATUS`(16)'s state bits.
    pub fn getstatus(&mut self, id: PortId) -> u32 {
        status_of_reply(&self.op(id, INET_REQ_GETSTATUS, b""))
    }

    /// `INET_REQ_GETFD`(14)'s descriptor.
    pub fn getfd(&mut self, id: PortId) -> i32 {
        fd_of_reply(&self.op(id, INET_REQ_GETFD, b""))
    }

    /// `INET_REQ_PEER`(4)'s packed address reply, verbatim.
    pub fn peer(&mut self, id: PortId) -> Vec<u8> {
        self.op(id, INET_REQ_PEER, b"")
    }

    /// `INET_REQ_NAME`(5)'s packed address reply, verbatim.
    pub fn name(&mut self, id: PortId) -> Vec<u8> {
        self.op(id, INET_REQ_NAME, b"")
    }

    /// `TCP_REQ_SHUTDOWN`(44) with one `TCP_SHUT_*` operand.
    pub fn shutdown(&mut self, id: PortId, how: u8) -> Vec<u8> {
        self.op(id, TCP_REQ_SHUTDOWN, &[how])
    }

    /// `TCP_REQ_SHUTDOWN`(44) with `TCP_SHUT_WR` — the `gen_tcp:shutdown(S, write)` operand.
    pub fn shutdown_write(&mut self, id: PortId) -> Vec<u8> {
        self.shutdown(id, TCP_SHUT_WR)
    }

    /// `INET_REQ_CLOSE`(2): the C's `tcp_close_check` + `tcp_desc_close`, which leave the **port**
    /// open but socket-less.
    pub fn close_socket(&mut self, id: PortId) -> Vec<u8> {
        self.op(id, INET_REQ_CLOSE, b"")
    }

    // -- the listener lifecycle ----------------------------------------------------------------

    /// `gen_tcp:listen`'s whole sequence: `open`(1) → `setopts`(7, `opts`) → `bind`(6,
    /// 127.0.0.1:0) → `listen`(27, `backlog`) → `name`(5), returning the kernel-assigned port.
    ///
    /// Asserts the whole ladder: `OPEN` is `[1]`, `bind` replies `[1, port_hi, port_lo]`,
    /// `listen` is `[1]` and `name` reports the very endpoint `bind` just allocated — which is
    /// what lets the foreign peer connect to a port nobody had to hard-code.
    pub fn listen(&mut self, id: PortId, opts: &[Vec<u8>], backlog: u16) -> u16 {
        assert_eq!(
            self.op(id, INET_REQ_OPEN, &open_arg()),
            ctl_ok(),
            "INET_REQ_OPEN(1) creates the socket"
        );
        if !opts.is_empty() {
            self.setopts(id, opts);
        }
        let bound = self.op(id, INET_REQ_BIND, &loopback_arg(0));
        assert_eq!(bound.len(), 3, "`bind` replies `[1, port_hi, port_lo]`, got {bound:?}");
        assert_eq!(bound[0], INET_REP_OK, "`bind` succeeds, got {bound:?}");
        let port = u16::from_be_bytes([bound[1], bound[2]]);
        assert_ne!(port, 0, "the kernel assigned an ephemeral port");
        assert_eq!(
            self.op(id, INET_REQ_LISTEN, &backlog.to_be_bytes()),
            ctl_ok(),
            "INET_REQ_LISTEN(27)"
        );
        assert_eq!(
            self.name(id),
            vec![INET_REP_OK, INET_AF_INET, bound[1], bound[2], 127, 0, 0, 1],
            "INET_REQ_NAME(5) reports the endpoint the socket is bound to"
        );
        port
    }

    /// Queues one `INET_REQ_ACCEPT`(26) with no deadline and returns the id its reply handed
    /// out (`[1] ++ <<Id:16/big>>`).
    pub fn queue_accept(&mut self, listener: PortId) -> u16 {
        let reply = self.op(
            listener,
            INET_REQ_ACCEPT,
            &accept_operand(INET_INFINITY),
        );
        let [tag, hi, lo] = reply[..] else {
            panic!("INET_REQ_ACCEPT answers `[1] ++ <<Id:16/big>>`, got {reply:?}");
        };
        assert_eq!(tag, INET_REP_OK, "the accept reply tag, got {reply:?}");
        u16::from_be_bytes([hi, lo])
    }

    /// Queues one passive `TCP_REQ_RECV`(42) with no deadline and no length limit, returning the
    /// id its reply handed out.
    pub fn queue_recv(&mut self, id: PortId) -> u16 {
        self.queue_recv_len(id, 0)
    }

    /// Queues one passive `TCP_REQ_RECV`(42) with no deadline and an explicit `Length` operand
    /// (`0` = whatever the socket buffered, the `prim_inet:recv(S, 0)` shape).
    pub fn queue_recv_len(&mut self, id: PortId, length: u32) -> u16 {
        let reply = self.op(id, TCP_REQ_RECV, &recv_operand(INET_INFINITY, length));
        let [tag, hi, lo] = reply[..] else {
            panic!("TCP_REQ_RECV answers `[1] ++ <<Id:16/big>>`, got {reply:?}");
        };
        assert_eq!(tag, INET_REP_OK, "the receive reply tag, got {reply:?}");
        u16::from_be_bytes([hi, lo])
    }

    /// Writes `payload` through the `prim_inet:send/4` command route: `[n:be16][tag][payload]`.
    ///
    /// Only the payload reaches the socket; the tag comes back verbatim on the
    /// `InetReply { caller_ref }` completion.
    pub fn send_command(&mut self, id: PortId, payload: &[u8]) {
        self.ports
            .port_command(
                &mut self.poller,
                id,
                &command(payload),
                PortCommandOptions::new(),
            )
            .expect("the command is queued");
    }

    /// Adopts a descriptor an accept handed over, exactly as `beam_rt`'s `adopt_accepted` route
    /// does: [`TcpInetDriver::from_accepted`] over the **listener's** driver (so the listener's
    /// option set is inherited), then a **second** port in the same registry.
    pub fn adopt(&mut self, listener: PortId, fd: i32) -> PortId {
        let driver = {
            let listener_driver = self
                .ports
                .driver_as::<TcpInetDriver>(listener)
                .expect("the listener port runs the tcp_inet driver");
            TcpInetDriver::from_accepted(fd, listener_driver)
                .expect("the accepted descriptor is adopted")
        };
        self.ports
            .open(&mut self.poller, Box::new(driver))
            .expect("the second port opens")
    }

    /// Connects an ordinary `std::net::TcpStream` — the **foreign peer**, never a second VM — to
    /// a loopback listener.
    pub fn client(&self, bound: u16) -> TcpStream {
        let peer = TcpStream::connect(("127.0.0.1", bound)).expect("the foreign peer connects");
        peer.set_nodelay(true).expect("nodelay is a socket option");
        peer
    }

    /// The whole accept loop for one connection: queue the accept, connect the peer, wait for the
    /// `Accepted { fd }` completion and adopt it. Returns `(connection port, peer, fd, id)`.
    pub fn accept_connection(&mut self, listener: PortId, bound: u16) -> (PortId, TcpStream, i32, u16) {
        let id = self.queue_accept(listener);
        let peer = self.client(bound);
        let drain = self.wait_for_completions(listener, "the queued accept completes");
        let (done_id, fd) = drain
            .accepted()
            .unwrap_or_else(|| panic!("the accept completion is a descriptor hand-off: {drain:?}"));
        assert_eq!(done_id, id, "the completion echoes the id its reply handed out");
        let connection = self.adopt(listener, fd);
        (connection, peer, fd, id)
    }

    // -- the rounds ----------------------------------------------------------------------------

    /// One reactor round: poll, dispatch the readiness (`handle_events` schedules *and* runs the
    /// round's port tasks), run the port tasks again (the round's `drive_async`/`TIMEOUT` point),
    /// then drain all four owner channels.
    pub fn round(&mut self) -> Round {
        self.rounds += 1;
        let events = match self.poller.poll(Some(POLL_SLICE)) {
            Ok(events) => events,
            // A wait the kernel interrupted is not a failure (`mio` does not retry `EINTR`).
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => Vec::new(),
            Err(err) => panic!("poll failed: {err:?}"),
        };
        let mut notifications = self
            .ports
            .handle_events(&mut self.poller, &events)
            .expect("the ready events are dispatched")
            .notifications;
        notifications.extend(
            self.ports
                .execute_port_tasks(&mut self.poller, DEFAULT_READINESS_RED_BUDGET)
                .expect("the port tasks run")
                .notifications,
        );
        Round {
            events: events.len(),
            active: self.ports.take_active_inputs(),
            completions: self.ports.take_completions(),
            notifications,
            failures: self.ports.take_failures(),
        }
    }

    /// Pumps rounds until `done` holds over everything drained so far, under the hang guard.
    pub fn wait_until<F>(&mut self, what: &str, mut done: F) -> Drain
    where
        F: FnMut(&Drain) -> bool,
    {
        let deadline = Instant::now() + WAIT;
        let mut drain = Drain::default();
        while !done(&drain) {
            assert!(
                Instant::now() < deadline,
                "{what}: the condition never held within {WAIT:?} ({drain:?})"
            );
            drain.absorb(self.round());
        }
        drain
    }

    /// Pumps rounds until `port` has produced a completion, returning everything drained.
    pub fn wait_for_completions(&mut self, port: PortId, what: &str) -> Drain {
        self.wait_until(what, |drain| drain.completion_for(port).is_some())
    }

    /// Pumps rounds until `port` has produced a `Data` completion, and returns its read.
    pub fn wait_for_data(&mut self, port: PortId, what: &str) -> (Drain, ReadMessage) {
        let drain = self.wait_until(what, |drain| !drain.data_for(port).is_empty());
        let read = drain
            .data_for(port)
            .first()
            .map(|read| (*read).clone())
            .unwrap_or_else(|| panic!("{what}: no data completion"));
        (drain, read)
    }

    /// Pumps rounds until at least `want` `{active, …}` data deliveries have been drained.
    pub fn wait_for_deliveries(&mut self, want: usize, what: &str) -> Drain {
        self.wait_until(what, |drain| drain.data().len() >= want)
    }

    /// Pumps rounds for `budget`, asserting **nothing** was delivered on the `{active, …}` channel
    /// in any of them — the negative half of a case, kept honest by the rounds really running.
    pub fn assert_quiet(&mut self, budget: Duration, what: &str) -> Drain {
        let deadline = Instant::now() + budget;
        let mut drain = Drain::default();
        loop {
            let round = self.round();
            assert!(round.active.is_empty(), "{what}: {:?}", round.active);
            drain.absorb(round);
            if Instant::now() >= deadline {
                return drain;
            }
        }
    }

    /// Pumps rounds until the driver has latched `port`'s peer close
    /// ([`TcpInetDriver::peer_closed`]), returning everything drained on the way.
    pub fn wait_for_peer_close(&mut self, port: PortId, what: &str) -> Drain {
        let deadline = Instant::now() + WAIT;
        let mut drain = Drain::default();
        loop {
            let closed = self
                .ports
                .driver_as::<TcpInetDriver>(port)
                .is_some_and(TcpInetDriver::peer_closed);
            if closed {
                return drain;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: the peer close was never latched ({drain:?})"
            );
            drain.absorb(self.round());
        }
    }

    /// Closes a port and drives the reactor until it is really gone, so a **parked**
    /// ([`PortState::Closing`]) teardown cannot be mistaken for a leak or a finished close.
    pub fn close_port(&mut self, port: PortId) {
        if !self.ports.contains(port) {
            return;
        }
        self.ports
            .close(&mut self.poller, port)
            .expect("the port closes");
        let deadline = Instant::now() + WAIT;
        while self.ports.contains(port) {
            assert!(
                Instant::now() < deadline,
                "the port never finished closing"
            );
            let _ = self.round();
        }
    }
}

// ---------------------------------------------------------------------------
// The oracle: a real `tcp_inet` port under the system runtime
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and answers its stdout, or `None` — after a
/// loud [`skip`] — when `erl` is missing or does not answer within [`ERL_TIMEOUT`].
///
/// A non-zero exit is an **assertion failure**: that is the oracle speaking, and a broken oracle
/// session is a harness defect, never a skip.
pub fn run_erl(expr: &str) -> Option<String> {
    if !on_path("erl") {
        skip("the oracle check skipped: no `erl` on PATH");
        return None;
    }
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(Command::new("erl").arg("-noshell").arg("-eval").arg(code).output());
    });
    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            skip(&format!(
                "the oracle check skipped: `erl` did not answer within {ERL_TIMEOUT:?}"
            ));
            None
        }
        Ok(Err(err)) => {
            skip(&format!(
                "the oracle check skipped: could not spawn `erl` ({err}); is Erlang on PATH?"
            ));
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle expression failed (exit {:?}) — a non-zero exit is a divergence, \
                 never a skip:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// The Erlang side of a raw-driver cross-check: open a **real** port on `driver` with
/// `open_port({spawn_driver, …})` (exactly as `prim_inet.erl:103` does) and run `sequence` of
/// `(request code, payload)` calls, printing each reply as comma-separated bytes with `|` between
/// replies.
///
/// Byte lists are printed with `~s` over a pre-joined string, so a long reply (an option dump, a
/// wire image) is never wrapped by the formatter — that is what makes [`parse_dump`] safe.
pub fn erl_reply_dump(driver: &str, sequence: &[(u32, Vec<u8>)]) -> Option<Vec<Vec<u8>>> {
    let calls: Vec<String> = sequence
        .iter()
        .map(|(code, arg)| {
            let bytes: Vec<String> = arg.iter().map(u8::to_string).collect();
            format!("F({code}, <<{}>>)", bytes.join(","))
        })
        .collect();
    let expr = format!(
        "M = {CONTROL_MAGIC}, \
         P = open_port({{spawn_driver, \"{driver}\"}}, [binary]), \
         F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
         R = [{}], \
         S = fun(L) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- L])) end, \
         io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [S(L) || L <- R]))])",
        calls.join(", ")
    );
    let stdout = run_erl(&expr)?;
    let line = stdout
        .lines()
        .find(|line| line.contains('|'))
        .or_else(|| stdout.lines().find(|line| !line.trim().is_empty()))?;
    parse_dump(line)
}

/// Parses the oracle's `a,b,c|d,e|…` reply dump into one byte vector per reply.
pub fn parse_dump(line: &str) -> Option<Vec<Vec<u8>>> {
    let mut replies = Vec::new();
    for field in line.trim().split('|') {
        let mut bytes = Vec::new();
        for part in field.split(',') {
            if part.is_empty() {
                continue;
            }
            bytes.push(part.trim().parse::<u8>().ok()?);
        }
        replies.push(bytes);
    }
    Some(replies)
}

/// The value of one `key=value` line of an oracle probe's stdout.
///
/// The probes print **one short scalar per line** (`io:format("key=~p~n", …)` / `~s` over a byte
/// list), so a line scan is exact: the Erlang formatter never wraps a `~s` string, and a one-value
/// `~p` line stays inside its default 80-column budget.
pub fn field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .map(str::trim)
}
