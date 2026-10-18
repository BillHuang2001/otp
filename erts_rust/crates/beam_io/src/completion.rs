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

//! The **driver→owner completion side channel**: the `{inet_reply, …}` and
//! `{inet_async, …}` messages a socket driver owes the process that owns its port.
//!
//! # Why this is a separate channel
//!
//! The C driver API lets a driver *send a message* to the port's owner from inside a driver
//! callback — `erl_drv_send_term` / `driver_send_term` — and the inet driver uses that for
//! exactly two message families:
//!
//! * **`{inet_reply, Port, ok}` / `{inet_reply, Port, {error, Reason}}`**, optionally with the
//!   caller's tag as a fourth element: `inet_reply_am` / `inet_reply_error_am` /
//!   `inet_reply_finish` (`erts/emulator/drivers/common/inet_drv.c:2606-2680`,
//!   `inet_reply_am` at `:2668`, `inet_reply_error_am` at `:2719`, the tag append and send at
//!   `inet_reply_finish` `:2614-2637`). This is the **synchronous** reply of a
//!   `erlang:port_command/2` — a `gen_tcp:send/2` **blocks** in `receive {inet_reply,S,Status}`
//!   until it arrives (`erts/preloaded/src/prim_inet.erl`'s `send/4`, `sendto/4`), so a
//!   command's outcome can never be reported through a control-op return value.
//! * **`{inet_async, Port, Id, Result}`** — the completion of a queued asynchronous socket
//!   operation, where `Id` is the 16-bit id the control-op reply handed out and `Result` is
//!   `ok`, `{ok, Data}`, `{ok, Port2}` or `{error, Reason}`: `send_async_ok`
//!   (`inet_drv.c:2493-2508`), `send_async_ok_port` (`:2513-2534`), `send_async_error`
//!   (`:2539-2562`), drained by `async_ok`/`async_ok_port`/`async_error_am` and, on teardown,
//!   `async_error_am_all` (`:2597-2608`) which flushes **every** queued operation with
//!   `{error, closed}`.
//!
//! Both are a *message to the owner*, not a return value, so they cannot ride
//! [`PortNotification`](crate::port_task::PortNotification) either: that enum is the
//! `{Port, {data, Data}}` / port-exit seam and keeps exactly its two variants so an
//! exhaustive consumer (`beam_rt`) stays exhaustive. This is therefore a **third side
//! channel**, built exactly like the `{line, N}` one
//! ([`LineNotification`](crate::port_task::LineNotification)) and the `driver_output2`
//! split-read sink ([`DriverContext::deliver_read`](crate::driver::DriverContext::deliver_read)):
//! the driver *stages* a completion on its [`DriverContext`](crate::driver::DriverContext)
//! ([`DriverContext::deliver_completion`](crate::driver::DriverContext::deliver_completion)),
//! the port collects it, and the registry hands it to the runtime after the round
//! ([`PortRegistry::take_completions`](crate::registry::PortRegistry::take_completions)).
//!
//! # The channel's contract
//!
//! * **Delivered exactly once.** A staged completion is drained once and never re-reported; the
//!   port's queue is the single source of truth, exactly as the input byte queues are.
//! * **Never in a control-op return value.** A control op's reply bytes are the C's
//!   `port_control` return (`desc->caller` is a *separate* field for the message path), so a
//!   driver must not try to smuggle a reply through [`Driver::control`](crate::driver::Driver::control).
//! * **A caller must drain after every round**, like the line frames: the runtime polls
//!   [`PortRegistry::take_completions`] after each
//!   [`PortRegistry::execute_port_tasks`](crate::registry::PortRegistry::execute_port_tasks) /
//!   [`PortRegistry::handle_events`](crate::registry::PortRegistry::handle_events) round (and
//!   after any explicit driver interaction), or it loses them.
//! * **A close does not lose them.** Completion a driver stages while its port is being torn
//!   down — the C's `async_error_am_all` — is harvested into a registry-owned queue before the
//!   port is dropped, so it is still reported exactly once (see
//!   [`PortRegistry::close`](crate::registry::PortRegistry::close)).
//!
//! # Term-free by construction
//!
//! This crate never touches Erlang terms: the caller tag is carried **verbatim** as the bytes
//! of `term_to_binary(Mref, [local])` (the `prim_inet` caller-tag image, decode it with
//! `beam_codec`) and a reason is carried as the atom *name* it must become. The runtime builds
//! the actual message tuples.

use std::os::unix::io::RawFd;

use beam_process::PortId;

use crate::error;
use crate::port_task::ReadMessage;

/// The inet driver's `{error, Reason}` atom family — *why* a socket operation failed.
///
/// The C renders every failure as an atom: `error_atom(err)` is
/// `driver_mk_atom(errno_str(err))` (`erts/emulator/drivers/common/inet_drv.c:2314-2317`), so
/// an OS failure is the lowercase POSIX errno name (`econnreset`, `enotconn`, `epipe`, …), while
/// the two *driver* failures are the driver's own atoms `closed`
/// (`inet_reply_error_am(desc, am_closed)`, `:12690`, `:12801`, `:12861`) and `timeout`
/// (`inet_reply_error_am(INETP(desc), am_timeout)`, `:12480`).
///
/// This is deliberately a small closed vocabulary over an errno *number* rather than a string:
/// the crate stores no atom table, so the name is rendered where it is consumed
/// ([`InetReason::atom_name`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InetReason {
    /// The socket — or the port — is closed (`closed`).
    Closed,
    /// A queued operation did not complete in time (`timeout`).
    Timeout,
    /// An OS failure: the lowercase POSIX name of this errno.
    Errno(i32),
}

impl InetReason {
    /// The reason for an OS failure with `errno` — the C's `inet_reply_error(desc, err)` /
    /// `async_error(desc, err)`, both of which go through `error_atom(err)`.
    pub const fn from_errno(errno: i32) -> InetReason {
        InetReason::Errno(errno)
    }

    /// The **atom name** this reason becomes in Erlang.
    ///
    /// `Closed`/`Timeout` are their own names; an errno is spelled by the crate's single
    /// errno→name function ([`crate::error::errno_name`], the one reader of the `ERRNO_NAMES`
    /// table — the `erl_errno_id` rule of `erts/emulator/beam/erl_errno_str.c`), which also carries
    /// the C's own `errno_<N>` fallback for a code the table does not name
    /// (`ERTS_UNKNOWN_ERRNO_FORMAT`), so a reason is never silently dropped. Nothing here spells an
    /// errno name of its own: there is exactly one such place in the crate
    /// ([`crate::error`]).
    pub fn atom_name(&self) -> String {
        match self {
            InetReason::Closed => "closed".to_string(),
            InetReason::Timeout => "timeout".to_string(),
            InetReason::Errno(errno) => error::errno_name(*errno),
        }
    }
}

/// The reply half of an [`CompletionEvent::InetReply`] — the C's `inet_reply_ok` (`ok`) vs
/// `inet_reply_error_am` (`{error, Reason}`).
///
/// Those are the only two shapes the inet driver produces (`inet_reply_am` is only ever called
/// with `am_ok`, through `inet_reply_ok`, `inet_drv.c:2691`), which is what
/// `prim_inet:send/4` matches: `{inet_reply,S,ok,Mref}` is a partial write it retries, every
/// other status is the command's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InetReply {
    /// The command was accepted by the OS (`ok`).
    Ok,
    /// The command failed: `{error, Reason}`.
    Error(InetReason),
}

/// The result half of a [`CompletionEvent::InetAsync`] — the C's `send_async_*` family's fourth
/// tuple element.
///
/// # The data plane is a [`ReadMessage`], not a byte vector
///
/// [`InetAsyncResult::Data`] carries the crate's existing read shape rather than a flat `Vec<u8>`
/// because the C's `inet_async_data` (`inet_drv.c:3162`) builds a *term* whose element count
/// depends on a header/body split:
///
/// * **TCP, `[binary]` mode, `hsz == 0`** (the default — `desc->inet.hsz` is only ever set by
///   `{header, N}`): the payload is one binary, i.e. `header` empty and `body = Some(payload)`.
/// * **TCP, `[list]` mode**: the C takes its `hsz > len` branch, `LOAD_STRING`, and the owner sees
///   a **flat** int list — the runtime renders `read.flattened()`.
/// * **UDP**: `packet_inet_input` splits the received bytes into `nsz` address bytes and `len`
///   payload bytes (`:15560`), so `header` is the packed source address
///   ([`pack_address`](crate::drivers::inet::wire)) and `body` is the datagram. In `[list]` mode
///   the result is one flat list with the address first; in `[binary]` mode it is the **improper**
///   list `[AF, PortHi, PortLo, A, B, C, D | <<Payload>>]`.
///
/// Which of the two the owner sees is decided above this crate (the port's `{mode, list|binary}`
/// option); this crate only carries the split, exactly as it does for
/// [`ReadMessage`](crate::port_task::ReadMessage) on the `{Port, {data, …}}` path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InetAsyncResult {
    /// `ok` (`send_async_ok`, `inet_drv.c:2493-2508`).
    Ok,
    /// `{ok, Data}`: one completed read, with the C's header/body split preserved.
    Data(ReadMessage),
    /// `{ok, NewPort}`: the **descriptor** of a connection an `accept` operation took off a
    /// listening socket, handed to the runtime so it can open the second port itself.
    ///
    /// The C creates that port inside the driver (`tcp_inet_copy`, `inet_drv.c:11834`) and answers
    /// `{ok, Port2}` through `async_ok_port` (`:2513-2534`); a driver in this crate cannot create a
    /// port, so the descriptor crosses the channel and the runtime performs the adoption
    /// (`TcpInetDriver::from_socket`, or the already-served `INET_REQ_FDOPEN` request).
    ///
    /// # Ownership (the hand-off rule)
    ///
    /// The descriptor is **the consumer's** the moment the completion is staged: the accepting
    /// driver hands it straight to
    /// [`DriverContext::deliver_completion`](crate::driver::DriverContext::deliver_completion)
    /// without ever storing it, so nothing remembers it, and no teardown path
    /// ([`Driver::stop`](crate::driver::Driver::stop), `Drop`) can ever `close(2)` it. The owner
    /// must either adopt it (the runtime's job) or close it; it is never closed here. A completion
    /// that reports a failure instead ([`InetAsyncResult::Error`], e.g. `{error, timeout}`) carries
    /// no descriptor because none was ever taken off the listening socket.
    Accepted {
        /// The accepted, **non-blocking** connection descriptor, owned by the completion's
        /// consumer.
        fd: RawFd,
    },
    /// `{ok, Port2}`: the second port an operation created (`send_async_ok_port`, `:2513-2534`).
    /// The C's own shape; no driver in this crate produces it (see
    /// [`InetAsyncResult::Accepted`] for what a Rust `accept` reports).
    Port(PortId),
    /// `{error, Reason}` (`send_async_error`, `:2539-2562`).
    Error(InetReason),
}

/// What a driver hands up for its port's owner.
///
/// A driver constructs one of these and passes it to
/// [`DriverContext::deliver_completion`](crate::driver::DriverContext::deliver_completion) — the
/// port stamps its own [`PortCompletion::port`], so a driver can never mis-attribute a
/// completion — and the runtime drains it with
/// [`PortRegistry::take_completions`](crate::registry::PortRegistry::take_completions).
///
/// Delivering to the **owner** is the runtime's job: this crate has no pid, and the C's
/// `desc->caller` is the process whose control op asked for the operation. `beam_rt` renders
/// [`CompletionEvent::InetReply`] as
/// `{inet_reply, Port, ok}` / `{inet_reply, Port, {error, Reason}}` (a 4-tuple with the decoded
/// `caller_ref` appended when it is `Some`, `inet_reply_finish`, `inet_drv.c:2614-2637`) and
/// [`CompletionEvent::InetAsync`] as `{inet_async, Port, Id, Result}`, both addressed to the
/// port's owner.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CompletionEvent {
    /// The answer to a `erlang:port_command/2` — the C's `{inet_reply, Port, Reply[, Tag]}`.
    InetReply {
        /// The caller's tag: `desc->caller_ref`, the `term_to_binary(Mref, [local])` image the
        /// command payload carried, or `None` when the caller sent none. It is carried
        /// **verbatim** (a 131-led External Term Format image, not a decoded term): the runtime
        /// decodes it into the reference the `prim_inet` caller matches on. When present the C
        /// appends it as a fourth tuple element (`inet_reply_finish`, `:2614-2637`) — so the
        /// owner sees `{inet_reply, Port, Reply, Mref}` — and when absent the tuple stays 3-wide
        /// (`inet_reply_caller_ref` answers the plain 3-tuple with `undefined`, `:2642-2664`).
        caller_ref: Option<Vec<u8>>,
        /// `ok`, or the `{error, Reason}` the driver reports.
        reply: InetReply,
    },
    /// The completion of a queued asynchronous operation — the C's
    /// `{inet_async, Port, Id, Result}`.
    InetAsync {
        /// The 16-bit id the operation's control-op reply handed out (the C's `int Ref`, `id` in
        /// `enq_async`): the caller matches it against the id it is waiting for.
        id: u16,
        /// What the operation produced.
        result: InetAsyncResult,
    },
}

impl CompletionEvent {
    /// The `{inet_reply, Port, ok}` completion an accepted command produces
    /// (`inet_reply_ok`, `inet_drv.c:2689-2692`).
    pub fn reply_ok(caller_ref: Option<Vec<u8>>) -> CompletionEvent {
        CompletionEvent::InetReply {
            caller_ref,
            reply: InetReply::Ok,
        }
    }

    /// The `{inet_reply, Port, {error, Reason}}` completion a refused or failed command produces
    /// (`inet_reply_error_am`, `inet_drv.c:2719-2741`).
    pub fn reply_error(caller_ref: Option<Vec<u8>>, reason: InetReason) -> CompletionEvent {
        CompletionEvent::InetReply {
            caller_ref,
            reply: InetReply::Error(reason),
        }
    }

    /// Whether this is an `{inet_reply, …}` completion.
    pub fn is_inet_reply(&self) -> bool {
        matches!(self, CompletionEvent::InetReply { .. })
    }

    /// Whether this is an `{inet_async, …}` completion.
    pub fn is_inet_async(&self) -> bool {
        matches!(self, CompletionEvent::InetAsync { .. })
    }
}

/// One completion, bound to the port that produced it.
///
/// This is what [`PortRegistry::take_completions`](crate::registry::PortRegistry::take_completions)
/// hands the runtime: the [`port`](PortCompletion::port) field is stamped by the port itself when
/// the driver delivers ([`DriverContext::deliver_completion`](crate::driver::DriverContext::deliver_completion)),
/// so a driver cannot attribute a completion to another port.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PortCompletion {
    /// The port whose driver produced the completion (the C's `desc->dport`).
    pub port: PortId,
    /// What the driver handed up.
    pub event: CompletionEvent,
}

impl PortCompletion {
    /// Binds `event` to `port`.
    pub const fn new(port: PortId, event: CompletionEvent) -> PortCompletion {
        PortCompletion { port, event }
    }
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod completion_tests;
