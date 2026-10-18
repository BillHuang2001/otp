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

//! The [`Port`] — a process-like I/O endpoint — plus its lifecycle, control and command
//! vocabularies.
//!
//! A port owns a [`Driver`], an **input** byte queue, an **output** byte queue, a
//! [`PortTaskQueue`] and a lifecycle ([`PortState`]). Port ids are allocated by
//! `beam_process::PortTable` (never a second number allocator); the set of live ports, the id
//! allocator and the reactor glue that schedules and runs their tasks live in the sibling
//! [`crate::registry`] module ([`PortRegistry`]).
//!
//! The legacy C analogue is the `Port`/`erts_port` control block and its `ioq`
//! (`erts/emulator/beam/erl_port.h`, `io.c`). The *scheduler* wiring (port run queues, poll
//! threads, reductions charged to a scheduler) still belongs to `beam_process`/`beam_rt`.
//!
//! # Single-thread invariant (enforced)
//!
//! Ports are **not** thread-safe and this crate has no locking: [`Port`] and [`PortRegistry`]
//! must be created and driven from **one OS thread**, and concurrent registration or dispatch
//! from several threads is unsupported. The invariant is enforced by the type system rather
//! than by documentation alone: [`PortRegistry`] carries a
//! [`PhantomData<core::cell::Cell<()>>`](core::marker::PhantomData) marker, so it is neither
//! `Send` nor `Sync` and cannot be moved into or shared with another thread. (A `Port` already
//! holds a `Box<dyn Driver>` with no `Send`/`Sync` bound and is therefore single-threaded by
//! construction.)
//!
//! # Implementation note: destructuring a `Port`
//!
//! A [`Port`] method that calls into its driver must build a [`DriverContext`] from
//! **disjoint** fields of `self`: destructure first, e.g.
//!
//! ```ignore
//! let Port { driver, input, output, id, .. } = self;
//! let mut ctx = DriverContext::new(*id, poller, input, output);
//! driver.output(&mut ctx)
//! ```
//!
//! so the driver (`driver`) and its context (`poller`, `input`, `output`) borrow separate
//! members of the struct. Do **not** add a combined accessor that hands out `&mut` to both
//! the driver and the queues at once — that cannot borrow-check.

use std::collections::VecDeque;
use std::time::Instant;

use beam_common::Result;
use beam_process::PortId;

use crate::driver::{Driver, DriverContext, PortArms, PortExitReason};
use crate::drivers::line_buffer::LineBuffer;
use crate::error;
use crate::poller::Poller;
use crate::port_task::{
    LineNotification, PortNotification, PortTask, PortTaskKind, PortTaskQueue, ReadMessage,
    PORT_REDS_EXECUTE,
};
use crate::queue::ByteQueue;

// The registry lives in the sibling [`crate::registry`] module; re-export it here so the
// in-crate `crate::port::PortRegistry` path (used by the built-in drivers) keeps resolving.
pub use crate::registry::PortRegistry;

/// The reduction budget [`PortRegistry::handle_events`] gives the tasks it schedules when
/// the caller does not drive the queue through [`PortRegistry::execute_port_tasks`] itself.
///
/// It is one whole port execution round (`ERTS_PORT_REDS_EXECUTE`), so a single batch of
/// poller events can run at most [`crate::port_task::PORT_REDS_EXECUTE`] reductions' worth of
/// port tasks; anything left over stays queued for the next round.
pub const DEFAULT_READINESS_RED_BUDGET: usize = PORT_REDS_EXECUTE;
/// The lifecycle state of a port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortState {
    /// Allocated but the driver's `start` has not completed.
    Opening,
    /// Started and able to move data.
    Open,
    /// Closing: the driver is shutting down.
    Closing,
    /// Fully closed; the id has been released.
    Closed,
}

/// A single I/O endpoint: a driver plus its input/output byte queues and task queue.
pub struct Port {
    /// This port's id (from `beam_process::PortTable`).
    id: PortId,
    /// The lifecycle state.
    state: PortState,
    /// The owning driver.
    driver: Box<dyn Driver>,
    /// Bytes read from the device, waiting for Erlang.
    input: ByteQueue,
    /// Bytes written by Erlang, waiting for the device.
    output: ByteQueue,
    /// The tasks scheduled on this port, in kind-priority order.
    tasks: PortTaskQueue,
    /// Which readiness directions the driver has registered with the poller (see
    /// [`PortArms`]); the reactor only schedules a direction the driver can serve.
    arms: PortArms,
    /// Set once a [`PortExitReason`] has been delivered to the driver, so
    /// [`Port::notify_exit`] delivers the port-exit notification **at most once**.
    exit_notified: bool,
    /// The line-framing mode (`{line, N}`): `None` delivers raw bytes
    /// ([`PortNotification::Data`]), `Some(max_len)` splits the input into
    /// [`LineNotification`] frames of at most `max_len` bytes. The C's
    /// `ERTS_PORT_SFLG_LINEBUF_IO` plus `linebuf->max_len` (`erl_port.h:330`).
    line_mode: Option<usize>,
    /// The line accumulator used in line mode (the C's `allocate_linebuf` state; untouched
    /// while [`Port::line_mode`] is `None`).
    line: LineBuffer,
    /// The **completed read messages** the driver handed up and the owner has not been told
    /// about yet, in arrival order: one [`ReadMessage`] per driver `driver_output*` call (see
    /// [`DriverContext::deliver_read`]) plus, once a frame is closed
    /// ([`Port::close_input_frame`]), one body-only message per batch of [`Port::input`]
    /// bytes. A [`PortNotification::Data`] (raw mode) and the line frames (line mode) are
    /// both produced from this queue, in this order.
    pending_reads: VecDeque<ReadMessage>,
    /// The line frames staged for the owner in line mode, in arrival order. Populated by
    /// [`Port::take_input_notifications`] (which frames the buffered input) and drained by
    /// [`Port::take_line_frames`] — the separate channel a `{line, N}` port reports through,
    /// so its input never rides [`PortNotification`].
    pending_lines: VecDeque<LineNotification>,
}

impl Port {
    /// Builds a port in the [`PortState::Opening`] state around `driver`.
    ///
    /// Called by [`PortRegistry::open`]; not part of the public API.
    pub(crate) fn new(id: PortId, driver: Box<dyn Driver>) -> Port {
        Port {
            id,
            state: PortState::Opening,
            driver,
            input: ByteQueue::new(),
            output: ByteQueue::new(),
            tasks: PortTaskQueue::new(),
            arms: PortArms::default(),
            exit_notified: false,
            line_mode: None,
            line: LineBuffer::new(0),
            pending_reads: VecDeque::new(),
            pending_lines: VecDeque::new(),
        }
    }
    /// This port's id.
    pub fn id(&self) -> PortId {
        self.id
    }

    /// This port's lifecycle state.
    pub fn state(&self) -> PortState {
        self.state
    }

    /// The registered name of the port's driver.
    pub fn driver_name(&self) -> &str {
        self.driver.name()
    }

    /// The number of bytes currently buffered in the input queue.
    ///
    /// This is **all** the input the port owes its owner, in arrival order: every completed
    /// read message's bytes (its header bytes included — the C's `bytes_in`) followed by the
    /// bytes banked in the open frame's accumulator. It is what the oracle's
    /// `port_info(P, input)` counts, header included and undelivered bytes included.
    pub fn input_len(&self) -> usize {
        self.pending_reads.iter().map(ReadMessage::len).sum::<usize>() + self.input.len()
    }

    /// Copies up to `out.len()` buffered input bytes into `out`, returning how many were
    /// copied.
    ///
    /// The bytes are taken in arrival order (completed read messages first, then the open
    /// frame's accumulator) and consumed, exactly as [`Port::drain_input`] does; only the
    /// header/body split is dropped by this flat view.
    pub fn read_input(&mut self, out: &mut [u8]) -> usize {
        let mut written = 0;
        while written < out.len() {
            let Some(read) = self.pending_reads.front_mut() else {
                break;
            };
            let room = out.len() - written;
            if !read.header.is_empty() {
                let n = read.header.len().min(room);
                out[written..written + n].copy_from_slice(&read.header[..n]);
                read.header.drain(..n);
                written += n;
                continue;
            }
            match read.body.as_mut() {
                Some(body) if body.is_empty() => {
                    // A zero-byte message (an explicit empty read): nothing to copy.
                    self.pending_reads.pop_front();
                }
                Some(body) => {
                    let n = body.len().min(room);
                    out[written..written + n].copy_from_slice(&body[..n]);
                    body.drain(..n);
                    written += n;
                }
                None => {
                    // A header-only read whose header was just consumed.
                    self.pending_reads.pop_front();
                }
            }
        }
        if written < out.len() {
            written += self.input.read_into(&mut out[written..]);
        }
        written
    }

    /// Removes and returns every buffered input byte, in arrival order.
    ///
    /// This is the flat view of [`Port::input_len`]: every completed read message (header
    /// bytes first, then the body) followed by the accumulator, so a caller that only wants
    /// the byte stream — the built-in drivers' tests, `{packet, N}` framing, … — sees exactly
    /// the concatenation of everything the driver handed up.
    pub fn drain_input(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.input_len());
        for read in self.pending_reads.drain(..) {
            out.extend_from_slice(&read.header);
            if let Some(body) = read.body {
                out.extend_from_slice(&body);
            }
        }
        out.extend_from_slice(self.input.front());
        self.input.clear();
        out
    }
    /// The number of bytes currently buffered in the output queue.
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// Starts the port: builds a context and calls the driver's `start`.
    ///
    /// The port's `TIMEOUT` task is then armed from [`Driver::next_timeout`] (see
    /// [`Port::refresh_timeout`]), so a driver that declares a deadline in `start` gets its
    /// timer.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `start` returns.
    pub fn open(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        let result = {
            // Destructure first: the driver and its context borrow disjoint fields.
            let Port {
                driver,
                input,
                output,
                arms,
                pending_reads,
                ..
            } = self;
            let mut ctx =
                DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
            driver.start(&mut ctx)
        };
        self.refresh_timeout();
        result?;
        self.state = PortState::Open;
        Ok(())
    }

    /// Appends `data` to the output queue **without** asking the driver to write anything.
    ///
    /// The queued bytes are handed to the device by the driver's `output`/`ready_output`,
    /// which [`Port::output`] runs inline and an `OUTPUT` [port task](PortTaskKind) runs
    /// when it is executed.
    pub fn queue_output(&mut self, data: &[u8]) {
        self.output.push(data);
    }

    /// Queues `data` for output and asks the driver to drain it **inline**.
    ///
    /// Returns the number of bytes the driver actually wrote; the remainder stays queued. The
    /// port's `TIMEOUT` task is re-armed from [`Driver::next_timeout`] afterwards (see
    /// [`Port::refresh_timeout`]), and a write that leaves the output queue **non-empty**
    /// schedules an [`PortTaskKind::Output`] task — so a driver that does not re-arm write
    /// readiness cannot strand the remainder: the next port-task round re-drives the write
    /// (exactly what [`crate::PortRegistry::port_command`] gets from scheduling its own task).
    ///
    /// The driver's byte count is checked against the queue through
    /// [`Port::check_write_accounting`] — see that method for the contract.
    ///
    /// # A refused command
    ///
    /// A driver reports a refusal as an **error**, and that error short-circuits
    /// `check_write_accounting`: the reported byte count is never inspected, `data` stays in
    /// the output queue (**not** counted as written) and no [`PortTaskKind::Output`] task is
    /// scheduled for the remainder.
    ///
    /// That is deliberate, and it is where this layer's grammar differs from the C's. The C
    /// cannot express a refused command for a loaded driver at all: `drv->output`/`outputv`
    /// are `void`, so the emulator counts the whole command as written
    /// (`prt->bytes_out += size`) and frees it unconditionally (`call_driver_output`,
    /// `erts/emulator/beam/io.c:1517,1520`; the free is `port_sig_output`'s
    /// `cleanup_scheduled_output`, `:1571`). Here the write path is queue-then-drive
    /// ([`Port::queue_output`] queues, the driver is asked to drain), so a refusal necessarily
    /// leaves the command in the queue as a Rust-side artefact. A caller that sees the error
    /// must therefore treat the command as **refused** — raise it, as the oracle does for
    /// `port_command/2` on a socket-less `tcp_inet`/`udp_inet` port — and must not re-drive the
    /// port: no task retries the bytes, and they disappear with the port (a
    /// [`crate::PortRegistry::close`] drops it, and both queues with it). There is deliberately
    /// no "discard the remainder" seam on this path.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `output` returns, or the typed accounting diagnostic when
    /// the driver reports a byte count it did not consume.
    pub fn output(&mut self, poller: &mut Poller, data: &[u8]) -> Result<usize> {
        self.queue_output(data);
        let before = self.output.len();
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                pending_reads,
                ..
            } = self;
            let mut ctx =
                DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
            driver.output(&mut ctx)
        };
        self.refresh_timeout();
        let written = self.check_write_accounting(before, result)?;
        if !self.output.is_empty() {
            self.schedule_task(PortTaskKind::Output);
        }
        Ok(written)
    }

    /// Schedules a task of `kind`, reporting whether it was newly scheduled.
    ///
    /// At most one task of a kind is ever pending (see [`PortTaskQueue::enqueue`]), so a
    /// repeated call while the task is still queued is a no-op.
    pub fn schedule_task(&mut self, kind: PortTaskKind) -> bool {
        self.tasks.enqueue(kind)
    }

    /// Schedules a [`PortTaskKind::Timeout`] task that may not run before `deadline`.
    pub fn schedule_timeout(&mut self, deadline: Instant) -> bool {
        self.tasks.enqueue_timeout(deadline)
    }

    /// Arms this port's `TIMEOUT` task at `deadline`, **replacing** any deadline already armed.
    ///
    /// This is the explicit counterpart of the automatic re-arm from [`Driver::next_timeout`]
    /// (see [`Port::refresh_timeout`]), and the one place a caller can re-point the timer: it
    /// always (re-)targets the port's single `TIMEOUT` task, unlike
    /// [`Port::schedule_timeout`], which is a no-op while a timer is already pending. The task
    /// still only runs once `deadline` has passed (see [`PortTaskQueue::pop_next`]).
    pub fn arm_timeout(&mut self, deadline: Instant) {
        self.tasks.arm_timeout(deadline);
    }

    /// Drops any pending `TIMEOUT` task, reporting whether one was pending.
    ///
    /// A later driver interaction re-derives the timer from [`Driver::next_timeout`], which
    /// may re-arm it (see [`Port::refresh_timeout`]).
    pub fn cancel_timeout(&mut self) -> bool {
        self.tasks.remove(PortTaskKind::Timeout)
    }

    /// The deadline the driver currently wants its `timeout` callback at, if any.
    ///
    /// Reads [`Driver::next_timeout`] directly; the port's `TIMEOUT` task mirrors this value
    /// after each driver interaction (see [`Port::refresh_timeout`]).
    pub fn next_timeout(&self) -> Option<Instant> {
        self.driver.next_timeout()
    }

    /// Re-arms (or clears) this port's `TIMEOUT` task from the driver's declared deadline.
    ///
    /// Called **after every driver interaction** (`start`/`output`/`ready_input`/
    /// `ready_output`/`control`/`timeout`), so the `TIMEOUT` task always mirrors
    /// [`Driver::next_timeout`]: a declared deadline arms (or re-points) the timer, and no
    /// declared deadline clears it. The deadline is a lower bound — an armed task whose
    /// deadline has not passed stays queued ([`PortTaskQueue::pop_next`]), so the driver's
    /// `timeout` never runs early.
    pub(crate) fn refresh_timeout(&mut self) {
        match self.driver.next_timeout() {
            Some(deadline) => self.tasks.arm_timeout(deadline),
            None => {
                self.tasks.remove(PortTaskKind::Timeout);
            }
        }
    }

    /// Whether the driver has armed a registration that wants readability.
    ///
    /// The reactor consults this before scheduling an [`PortTaskKind::Input`] task, so a
    /// direction the driver never registered cannot be served (see [`PortArms`]).
    pub(crate) fn read_armed(&self) -> bool {
        self.arms.read_armed()
    }

    /// Whether the driver has armed a registration that wants writability.
    ///
    /// The reactor consults this before scheduling an [`PortTaskKind::Output`] task (see
    /// [`PortArms`]).
    pub(crate) fn write_armed(&self) -> bool {
        self.arms.write_armed()
    }

    /// The number of tasks currently scheduled on this port.
    pub fn pending_tasks(&self) -> usize {
        self.tasks.len()
    }

    /// The kind of the task that would run next, if any.
    pub fn next_task(&self) -> Option<PortTaskKind> {
        self.tasks.next()
    }

    /// Whether a task of `kind` is scheduled on this port.
    pub fn has_pending_task(&self, kind: PortTaskKind) -> bool {
        self.tasks.contains(kind)
    }

    /// Removes and returns the task that may run at `now`, if there is one.
    pub(crate) fn pop_ready_task(&mut self, now: Instant) -> Option<PortTask> {
        self.tasks.pop_next(now)
    }

    /// Runs one scheduled `task` against the driver, returning the reductions it cost.
    ///
    /// `INPUT` runs the driver's `ready_input`; `OUTPUT` runs its `ready_output` (which the
    /// driver may define in terms of `output`); `TIMEOUT` runs its `timeout`.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver callback returns.
    pub(crate) fn run_task(&mut self, poller: &mut Poller, task: &PortTask) -> Result<usize> {
        match task.kind() {
            PortTaskKind::Input => {
                self.ready_input(poller)?;
                Ok(PortTaskKind::Input.reductions())
            }
            PortTaskKind::Output => {
                let _written = self.ready_output(poller)?;
                Ok(PortTaskKind::Output.reductions())
            }
            PortTaskKind::Timeout => {
                self.timeout(poller)?;
                Ok(PortTaskKind::Timeout.reductions())
            }
        }
    }

    /// Enables or disables **line framing** (`{line, N}`) for this port's input.
    ///
    /// `Some(max_len)` turns line mode on with an at-most-`max_len`-byte frame length (`0` =
    /// no length limit); `None` turns it off, so the driver's bytes are delivered raw as
    /// [`PortNotification::Data`]. This is the C's `open_port(..., [line])` /
    /// `[binary, {line, N}]` option (`ERTS_PORT_SFLG_LINEBUF_IO` +
    /// `allocate_linebuf(max_len)`, `erl_bif_port.c:862-869,986`), which the runtime applies
    /// once the port is open.
    ///
    /// Switching **does not flush or discard** buffered input: bytes the driver already read
    /// stay in the line accumulator and are delivered — framed if line mode is still on, raw
    /// otherwise — by the next harvest (or by [`Port::flush_linebuf`]).
    pub fn set_line_mode(&mut self, max_len: Option<usize>) {
        self.line_mode = max_len;
        if let Some(max_len) = max_len {
            self.line.set_max_len(max_len);
        }
    }

    /// This port's line-framing mode: `None` (the default) delivers raw bytes, `Some(max_len)`
    /// splits the input into at-most-`max_len`-byte frames (see [`Port::set_line_mode`]).
    pub fn line_mode(&self) -> Option<usize> {
        self.line_mode
    }

    /// Releases the pending partial line as `noeol` frame(s) for the next harvest.
    ///
    /// This is the C's `flush_linebuf`/`flush_linebuf_messages` (`io.c:7221`), which the port
    /// runs from `driver_eof`: when a `{line, N}` port's input stream ends, the bytes held
    /// back because no `\n` had arrived yet are delivered **once** as a `noeol` frame
    /// (`{Port, {data, {noeol, Data}}}`) — the oracle's `printf 'a\nb'` ⇒ `{eol,"a"} {noeol,"b"}`.
    ///
    /// The frames are *staged* (see [`LineBuffer::flush_into_ready`]) rather than returned, so
    /// they ride the next [`Port::take_input_notifications`] together with any complete lines
    /// still queued — and are then drained from the [`Port::take_line_frames`] channel. A port
    /// in raw mode (`line_mode` is `None`) is untouched, and a repeat
    /// call stages nothing (a flush is idempotent), so this is safe to call on every
    /// end-of-file observation. [`Port::close`] never calls it: closing a port does **not**
    /// flush the partial line (only end-of-file does).
    pub fn flush_linebuf(&mut self) {
        if self.line_mode.is_none() {
            return;
        }
        // Any reads the driver handed up (and any bytes it banked) must be framed first, so
        // an end-of-file flush keeps its frames behind them in arrival order.
        self.stage_line_frames();
        self.line.flush_into_ready();
        // The flushed partial line is the C's `{noeol, "abc"}` — no header, it never came from
        // a `driver_output2` call of its own.
        let port = self.id;
        self.pending_lines
            .extend(self.line.take_frames().into_iter().map(|frame| {
                LineNotification {
                    port,
                    header: Vec::new(),
                    bytes: frame.bytes,
                    eol: frame.eol,
                }
            }));
    }

    /// Whether this port is holding input bytes it owes its owner.
    ///
    /// This covers both halves of the port's input state — the completed read messages and the
    /// open frame's accumulator — and is what decides whether a [`PortNotification::Data`] (or
    /// a line frame) is owed (see [`Port::take_input_notifications`]).
    pub(crate) fn has_input(&self) -> bool {
        !self.pending_reads.is_empty() || !self.input.is_empty()
    }

    /// Closes the **open input frame**: the bytes the driver banked on
    /// [`DriverContext::input`] become one body-only [`ReadMessage`], so they are reported
    /// before anything the driver hands up afterwards.
    ///
    /// This is the Rust analogue of the C delivering a `driver_output` the moment the driver
    /// calls it: [`DriverContext::deliver_read`] runs it before pushing its own read (so a
    /// banked push followed by a split read keeps their call order), and
    /// [`Port::take_input_notifications`] runs it once per harvest. An empty accumulator opens
    /// no frame at all, so no empty message is ever minted here.
    fn close_input_frame(&mut self) {
        close_input_frame(&mut self.input, &mut self.pending_reads);
    }

    /// Feeds every pending read through the line accumulator, stamping each frame it produces
    /// with that read's header, and stages the frames on the [`Port::take_line_frames`]
    /// channel.
    ///
    /// This is the line-mode half of [`Port::take_input_notifications`], shared with
    /// [`Port::flush_linebuf`]. Only a read's **body** is framed — the C's header is a cons
    /// prefix of the resulting message, not line content — and a read with no body (`None`)
    /// produces no frame at all.
    fn stage_line_frames(&mut self) {
        self.close_input_frame();
        let port = self.id;
        while let Some(read) = self.pending_reads.pop_front() {
            let Some(body) = read.body else {
                continue;
            };
            self.line.push(&body);
            let header = read.header;
            self.pending_lines
                .extend(self.line.take_frames().into_iter().map(|frame| {
                    LineNotification {
                        port,
                        header: header.clone(),
                        bytes: frame.bytes,
                        eol: frame.eol,
                    }
                }));
        }
    }

    /// Takes the input notifications this port owes, in arrival order.
    ///
    /// In raw mode ([`Port::line_mode`] is `None`) this is **one [`PortNotification::Data` per
    /// read message**: the C delivers one `{Port, {data, Data}}` per `driver_output*` call
    /// (`deliver_read_message`, `io.c:3363-3442`), so two calls inside one callback are two
    /// notifications and a header/body pair is exactly one — never two, never merged. Each
    /// message is drained out of the port, so a report happens **exactly once per byte**. An
    /// explicit empty read is still reported (the oracle's `{Port, {data, []}}`); the
    /// accumulator is closed into a body-only message first, so nothing is stranded.
    ///
    /// In line mode this returns **no** notification: the whole buffered input is fed through
    /// the line accumulator and every resulting frame is staged on the
    /// [`Port::take_line_frames`] channel instead, so a single read can yield many frames (a
    /// 601-byte `[line,4]` chunk is 150 of them: 149 `noeol` + 1 `eol`) — each stamped with the
    /// header of the read that completed it. An empty frame is **not** dropped here: a blank
    /// line is the oracle's `{Port, {data, {eol, []}}}`.
    ///
    /// The condition is the *queue* (and, in line mode, the line accumulator), not "an `INPUT`
    /// task ran": the legacy C delivers `driver_output` to the port owner as soon as the driver
    /// makes it, whatever callback it came from (`io.c`'s `driver_output`/`erts_port_output`),
    /// so a `driver_output` made from `start`/`control`/`timeout`/`output` — i.e. outside any
    /// `INPUT` task — must reach the owner on the next port-task round exactly like one made
    /// from `ready_input`. Gating the report on an `INPUT` task having run ([`Port::run_task`])
    /// would strand those bytes in the queue, which is why the gate was removed.
    ///
    /// In line mode the framing side effect happens even though the return value is empty, so a
    /// round must call this for every port (as [`crate::PortRegistry::execute_port_tasks`] does)
    /// before the frames can be drained with [`Port::take_line_frames`].
    pub(crate) fn take_input_notifications(&mut self) -> Vec<PortNotification> {
        if self.line_mode.is_none() {
            if !self.has_input() {
                return Vec::new();
            }
            self.close_input_frame();
            let port = self.id;
            return self
                .pending_reads
                .drain(..)
                .map(|read| PortNotification::Data { port, read })
                .collect();
        }
        self.stage_line_frames();
        Vec::new()
    }
    /// Takes the line frames this port owes, in arrival order.
    ///
    /// This is the drain side of the `{line, N}` channel: the frames
    /// [`Port::take_input_notifications`] staged out of the buffered input (plus any a
    /// `driver_eof` flush staged, see [`Port::flush_linebuf`]). An empty `eol` frame is kept —
    /// a blank line is the oracle's `{Port, {data, {eol, []}}}`. A port in raw mode never stages
    /// anything, so this is empty there.
    pub(crate) fn take_line_frames(&mut self) -> Vec<LineNotification> {
        self.pending_lines.drain(..).collect()
    }

    /// Dispatches read readiness to the driver's `ready_input`.
    ///
    /// The port's `TIMEOUT` task is re-armed from [`Driver::next_timeout`] afterwards (see
    /// [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `ready_input` returns.
    pub fn ready_input(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                pending_reads,
                ..
            } = self;
            let mut ctx =
                DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
            driver.ready_input(&mut ctx)
        };
        self.refresh_timeout();
        result
    }

    /// Dispatches write readiness to the driver's `ready_output`.
    ///
    /// The port's `TIMEOUT` task is re-armed from [`Driver::next_timeout`] afterwards (see
    /// [`Port::refresh_timeout`]) and the driver's byte count is checked against the output
    /// queue (see [`Port::check_write_accounting`]).
    ///
    /// A driver that **refuses** the write returns its error, which propagates from here with
    /// the accounting check short-circuited and the queued bytes left in the output queue (see
    /// [`Port::output`]'s *A refused command* section). This is the callback an
    /// [`PortTaskKind::Output`] task runs, so the refusal then surfaces out of the whole
    /// port-task round ([`crate::PortRegistry::execute_port_tasks`]) rather than as a
    /// [port-task outcome](crate::PortTaskOutcome): the task was already popped, so it is **not**
    /// re-queued and the refused command is never retried implicitly.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `ready_output` returns, or the typed accounting diagnostic
    /// when the driver reports a byte count it did not consume.
    pub fn ready_output(&mut self, poller: &mut Poller) -> Result<usize> {
        let before = self.output.len();
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                pending_reads,
                ..
            } = self;
            let mut ctx =
                DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
            driver.ready_output(&mut ctx)
        };
        self.refresh_timeout();
        self.check_write_accounting(before, result)
    }

    /// Checks a driver's write accounting: the output queue must have shrunk by exactly the
    /// number of bytes the driver reported.
    ///
    /// A driver drains [`DriverContext::output`] and returns how many bytes it took — calling
    /// `ctx.output().consume(n)` for every byte it counts. The queue is the single source of
    /// truth for what still has to be written, so a driver that reports a count it never
    /// consumed leaves those bytes queued and the **next** drive writes them again: duplicated
    /// output, silently. That is why the count is verified here rather than trusted — this is
    /// the guard every write path goes through ([`Port::output`] and [`Port::ready_output`],
    /// the latter also being the one [`PortTaskKind::Output`] runs).
    ///
    /// `before` is the queue length taken immediately before the callback, so the check is
    /// independent of what the driver did with the bytes: consuming fewer than reported, more
    /// than reported, or pushing more onto the queue are all the same mismatch. The diagnostic
    /// is typed (never a panic and never a silently truncated write), because only a driver
    /// implementation can produce it.
    ///
    /// # A driver error short-circuits the check
    ///
    /// The driver's outcome is unwrapped **first** (`let reported = result?`), so a driver that
    /// answers `Err` has its byte count discarded without being compared to the queue: a
    /// **refused** write is propagated verbatim and the command's bytes stay in the output
    /// queue, never counted as written ([`Port::output`] and [`Port::ready_output`] both go
    /// through here, the latter also being the one [`PortTaskKind::Output`] runs — see
    /// [`Port::output`]'s *A refused command* section for why the remainder is deliberately
    /// left queued). Only a driver that claims **success** is held to the accounting contract.
    ///
    /// # Errors
    ///
    /// Propagates the driver's own error, or returns the typed `unsupported` raise naming the
    /// reported count and the bytes actually consumed.
    fn check_write_accounting(&self, before: usize, result: Result<usize>) -> Result<usize> {
        let reported = result?;
        let consumed = before.saturating_sub(self.output.len());
        if consumed != reported {
            return Err(error::unsupported(&format!(
                "a port driver reported {reported} bytes written but consumed {consumed} from its output queue"
            )));
        }
        Ok(reported)
    }

    /// Dispatches a port-timer expiry to the driver's `timeout`.
    ///
    /// The port's `TIMEOUT` task is then re-armed from [`Driver::next_timeout`] (see
    /// [`Port::refresh_timeout`]), so a driver re-arms its timer by declaring the next deadline
    /// — no pending timer is left behind if it declares none.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `timeout` returns.
    pub fn timeout(&mut self, poller: &mut Poller) -> Result<()> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                pending_reads,
                ..
            } = self;
            let mut ctx =
                DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
            driver.timeout(&mut ctx)
        };
        self.refresh_timeout();
        result
    }

    /// Delivers a port-control command to the driver and returns its reply bytes.
    ///
    /// The port's `TIMEOUT` task is re-armed from [`Driver::next_timeout`] afterwards, so a
    /// driver that changes its timer in response to a control command (e.g. an idle timeout)
    /// takes effect immediately (see [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `control` returns.
    pub fn control(&mut self, poller: &mut Poller, command: &str, arg: &[u8]) -> Result<Vec<u8>> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                pending_reads,
                ..
            } = self;
            let mut ctx =
                DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
            driver.control(&mut ctx, command, arg)
        };
        self.refresh_timeout();
        result
    }

    /// Whether the driver has asynchronous work pending (a `driver_async` completion not yet handed
    /// over).
    ///
    /// This is the probe [`PortRegistry::execute_port_tasks`](crate::PortRegistry::execute_port_tasks)
    /// consults to decide whether the port needs a [`Port::drive_async`] point this round. It never
    /// touches the driver context, so it is safe to call while iterating the registry.
    pub fn has_pending_async(&self) -> bool {
        self.driver.has_pending_async()
    }

    /// Drives the driver's asynchronous completions, returning how many were processed.
    ///
    /// The port-task round has **no task kind** for `driver_async` completion in Milestone-1 (the
    /// C runs `ready_async` from `call_async_ready` on the port's scheduler), so the runtime calls
    /// this explicitly — mirroring every other driver interaction, the port's `TIMEOUT` task is
    /// re-armed from [`Driver::next_timeout`] afterwards (see [`Port::refresh_timeout`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's [`Driver::drive_async`] returns.
    pub fn drive_async(&mut self, poller: &mut Poller) -> Result<usize> {
        let id = self.id;
        let result = {
            let Port {
                driver,
                input,
                output,
                arms,
                pending_reads,
                ..
            } = self;
            let mut ctx =
                DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
            driver.drive_async(&mut ctx)
        };
        self.refresh_timeout();
        result
    }

    /// Takes the reaped exit status of the OS child this port owns, once.
    ///
    /// Forwards to the driver's [`Driver::take_exit_status`]: a port whose driver starts no
    /// child (the built-in `fd`/`efile` drivers) always answers `Ok(None)`, and so does a
    /// port whose child has not been reaped yet or whose status has already been taken (see
    /// [`crate::drivers::SpawnDriver`]).
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `take_exit_status` returns.
    pub fn take_exit_status(&mut self) -> Result<Option<i32>> {
        self.driver.take_exit_status()
    }

    /// Takes the port's end-of-file notification, once, and **flushes the line buffer**.
    ///
    /// Forwards to the driver's [`Driver::take_eof`]: `Ok(true)` means the port's input stream
    /// reached end-of-file and the port asked for the `eof` option — the signal a runtime turns
    /// into the bare `eof` message the oracle delivers. `Ok(false)` means "nothing to report"
    /// (no `eof` option, no end-of-file yet, or already reported); a port whose driver never
    /// streams input always answers `Ok(false)`.
    ///
    /// This is the crate's **driver-end-of-file observation point** (the C's `driver_eof`), so a
    /// report also runs the line-buffer flush: a `{line, N}` port's pending partial line is
    /// released as `noeol` frame(s) for the next harvest (see [`Port::flush_linebuf`]). The
    /// flush is only performed when the driver actually reports the end of the stream, so a
    /// caller polling this before then cannot release a line early; it is idempotent, so a
    /// second observation stages nothing.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `take_eof` returns.
    pub fn take_eof(&mut self) -> Result<bool> {
        let eof = self.driver.take_eof()?;
        if eof {
            self.flush_linebuf();
        }
        Ok(eof)
    }

    /// Tells the driver this port is exiting, **at most once**.
    ///
    /// Forwards `reason` to [`Driver::port_exit`] — the driver's advisory exit-notification
    /// channel, distinct from the teardown [`Driver::stop`] — and records that it was
    /// delivered, so a repeated call is a no-op that never reaches the driver again. The port
    /// stays live and its registrations stay armed; a later [`Port::close`] then runs `stop`,
    /// so the two are independent and a `port_exit` before a `stop` is well-defined.
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `port_exit` returns on the **first** call; a repeat never
    /// calls the driver (it returns `Ok(())`), so the first call's outcome is the only one
    /// observable.
    pub fn notify_exit(&mut self, poller: &mut Poller, reason: PortExitReason) -> Result<()> {
        if self.exit_notified {
            return Ok(());
        }
        // Record the delivery before calling the driver so a failed notification is still
        // delivered at most once (a repeat cannot re-run a driver callback with side effects).
        self.exit_notified = true;
        let id = self.id;
        let Port {
            driver,
            input,
            output,
            arms,
            pending_reads,
            ..
        } = self;
        let mut ctx = DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
        driver.port_exit(&mut ctx, reason)
    }

    /// Stops the driver and marks the port [`PortState::Closed`].
    ///
    /// # Errors
    ///
    /// Returns whatever the driver's `stop` returns.
    pub fn close(&mut self, poller: &mut Poller) -> Result<()> {
        self.state = PortState::Closing;
        let result = {
            let id = self.id;
            let Port {
                driver,
                input,
                output,
                arms,
                pending_reads,
                ..
            } = self;
            let mut ctx =
                DriverContext::new(id, poller, input, output, arms).with_reads(pending_reads);
            driver.stop(&mut ctx)
        };
        // The port is closed either way; a teardown failure must not leave it looking
        // open.
        self.state = PortState::Closed;
        result
    }
}

/// Closes the **open input frame**: the bytes banked in `input` become one body-only
/// [`ReadMessage`] pushed onto `reads`.
///
/// This is the single implementation of "the driver's plain `driver_output` bytes are now one
/// complete read", shared by [`Port::close_input_frame`] (the once-per-harvest close) and by
/// [`DriverContext::deliver_read`] (which closes the frame before its own split read, so the
/// C's synchronous delivery order survives). An empty accumulator closes nothing: an empty
/// read message is only ever minted by an explicit `deliver_read(&[], None)`.
pub(crate) fn close_input_frame(input: &mut ByteQueue, reads: &mut VecDeque<ReadMessage>) {
    if input.is_empty() {
        return;
    }
    let bytes = std::mem::replace(input, ByteQueue::new());
    reads.push_back(ReadMessage::body_only(bytes.front().to_vec()));
}

/// The **reserved** port-control operation namespace.
///
/// [`PortRegistry::port_control`] forwards every operation name it is handed **verbatim** to
/// the port's driver: the driver's `control` vocabulary (`"line"`/`"noline"`/`"binary"`/
/// `"text"`/`"getopts"` for the built-in `fd` driver, plus the `efile` family's
/// `"open"`/`"close"`/`"path"`/`"eof"` with an attached fd — see [`crate::drivers`]) is
/// authoritative, exactly as in the C `erts_port_control`. The registry itself never decodes
/// that vocabulary; it reserves **one** name before any driver sees it:
///
/// | Name | Meaning here |
/// |---|---|
/// | [`PortControl::CLOSE`] (`"close"`) | close the port (driver `stop` + id release) |
///
/// Every other name — recognised by a driver or not — is passed through unchanged, so an
/// unrecognised verb surfaces the driver's own [`beam_common::BeamError::BadArg`] rather than
/// being silently ignored here.
pub struct PortControl;

impl PortControl {
    /// The reserved `"close"` operation: closes the port like [`PortRegistry::close`] and
    /// answers with no bytes.
    pub const CLOSE: &'static str = "close";
}

/// The options `erlang:port_command/3` accepts, as a small flag set.
///
/// `nosuspend` and `force` mirror the Erlang options. Milestone-1 ports have **no** busy
/// state, no command queue and never suspend the caller (there is only one OS thread and
/// the command is queued unconditionally), so both flags are validated and carried but
/// change nothing yet: the runtime's `port_command/3` BIF layer owns their effect once a
/// busy-port command queue exists. They are modeled here rather than dropped so that the
/// flag plumbing is in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PortCommandOptions {
    /// The `nosuspend` option.
    nosuspend: bool,
    /// The `force` option.
    force: bool,
}

impl PortCommandOptions {
    /// The empty option set (`port_command/2`).
    pub const EMPTY: PortCommandOptions = PortCommandOptions {
        nosuspend: false,
        force: false,
    };

    /// The empty option set.
    pub const fn new() -> PortCommandOptions {
        PortCommandOptions::EMPTY
    }

    /// This option set with `nosuspend` set to `nosuspend`.
    pub const fn with_nosuspend(self, nosuspend: bool) -> PortCommandOptions {
        PortCommandOptions {
            nosuspend,
            force: self.force,
        }
    }

    /// This option set with `force` set to `force`.
    pub const fn with_force(self, force: bool) -> PortCommandOptions {
        PortCommandOptions {
            nosuspend: self.nosuspend,
            force,
        }
    }

    /// Whether the `nosuspend` option is set.
    pub const fn is_nosuspend(self) -> bool {
        self.nosuspend
    }

    /// Whether the `force` option is set.
    pub const fn is_force(self) -> bool {
        self.force
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver that banks a fixed payload on its first `ready_input` and reports the end of
    /// its input stream through the once-only [`Driver::take_eof`] seam.
    ///
    /// It registers no descriptor, so the port-level line-framing tests drive it directly
    /// (`ready_input` + `take_input_notifications`) — the framing never looks at the poller.
    struct BankBytesDriver {
        payload: Vec<u8>,
        fed: bool,
        eof_reported: bool,
    }

    impl BankBytesDriver {
        fn new(payload: &[u8]) -> BankBytesDriver {
            BankBytesDriver {
                payload: payload.to_vec(),
                fed: false,
                eof_reported: false,
            }
        }
    }

    impl Driver for BankBytesDriver {
        fn name(&self) -> &str {
            "bank-bytes-test"
        }

        fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
            Ok(0)
        }

        fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
            if !self.fed {
                self.fed = true;
                let payload = self.payload.clone();
                ctx.input().push(&payload);
            }
            Ok(())
        }

        fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn take_eof(&mut self) -> Result<bool> {
            if self.eof_reported {
                Ok(false)
            } else {
                self.eof_reported = true;
                Ok(true)
            }
        }
    }

    fn port_with(payload: &[u8]) -> (Poller, Port) {
        let poller = Poller::new().expect("a poll set");
        let port = Port::new(PortId::new(1), Box::new(BankBytesDriver::new(payload)));
        (poller, port)
    }

    /// The frames a port currently owes, as `(marker, bytes)` pairs: `None` is a raw
    /// [`PortNotification::Data`] chunk, `Some(eol)` a [`LineNotification`] frame.
    ///
    /// A round harvests both channels: [`Port::take_input_notifications`] reports the raw
    /// payloads (one per read message — a port can owe **several**, one per driver
    /// `driver_output*` call — and stages the line frames), [`Port::take_line_frames`] drains
    /// the staged line frames. A port is in exactly one mode, so the two never mix.
    fn frames(port: &mut Port) -> Vec<(Option<bool>, Vec<u8>)> {
        let mut frames: Vec<(Option<bool>, Vec<u8>)> = port
            .take_input_notifications()
            .into_iter()
            .map(|notification| match notification {
                PortNotification::Data { read, .. } => (None, read.flattened()),
                other => panic!("the port reported an unexpected notification: {other:?}"),
            })
            .collect();
        frames.extend(
            port.take_line_frames()
                .into_iter()
                .map(|frame| (Some(frame.eol), frame.bytes)),
        );
        frames
    }

    #[test]
    fn line_mode_is_off_until_it_is_set_and_then_reports_the_limit() {
        let (_poller, mut port) = port_with(b"x");
        assert_eq!(port.line_mode(), None, "raw mode is the default");
        port.set_line_mode(Some(4));
        assert_eq!(port.line_mode(), Some(4));
        port.set_line_mode(None);
        assert_eq!(port.line_mode(), None);
    }

    #[test]
    fn raw_mode_reports_the_whole_read_as_one_chunk() {
        let (mut poller, mut port) = port_with(b"a\nb\n");
        assert!(port.line_mode().is_none());
        port.ready_input(&mut poller).expect("the read succeeds");
        assert_eq!(frames(&mut port), vec![(None, b"a\nb\n".to_vec())]);
    }

    /// The partial-line flush is driven by **end of file**, never by a close: the C runs
    /// `flush_linebuf_messages` from `driver_eof` (`io.c:7211`), and `port_close` does not.
    ///
    /// This pins it on the port itself (not through the registry, which drops the port on
    /// close): the held partial line must survive the close and still be released by the
    /// explicit flush, which would be impossible had the close reported it.
    #[test]
    fn closing_a_line_port_never_flushes_the_pending_partial_line() {
        let (mut poller, mut port) = port_with(b"a\nb");
        port.set_line_mode(Some(0));
        port.ready_input(&mut poller).expect("the read succeeds");
        assert_eq!(
            frames(&mut port),
            vec![(Some(true), b"a".to_vec())],
            "the complete line is delivered, the terminator-less tail is held"
        );
        port.close(&mut poller).expect("the teardown succeeds");
        assert!(
            frames(&mut port).is_empty(),
            "a close must not flush the held partial line"
        );
        // …and the partial line is still held, so the `driver_eof` flush still releases it:
        // proof that the close neither reported nor discarded it.
        port.flush_linebuf();
        assert_eq!(frames(&mut port), vec![(Some(false), b"b".to_vec())]);
    }

    #[test]
    fn take_eof_runs_the_flush_and_a_close_free_port_reports_an_empty_line() {
        // A blank line is a **real** frame: the oracle delivers `{Port, {data, {eol, []}}}`, so
        // the empty payload must not be dropped the way raw mode drops it.
        let (mut poller, mut port) = port_with(b"\n");
        port.set_line_mode(Some(0));
        port.ready_input(&mut poller).expect("the read succeeds");
        assert_eq!(frames(&mut port), vec![(Some(true), Vec::new())]);
        // The stream's end is observed through `take_eof`, which stages a flush. Nothing is
        // held here, so the flush stages nothing and the report stays once-only.
        assert!(port.take_eof().expect("the check succeeds"));
        assert!(frames(&mut port).is_empty());
        assert!(!port.take_eof().expect("the check succeeds"));
    }

    #[test]
    fn flushing_a_raw_port_is_a_no_op() {
        let (mut poller, mut port) = port_with(b"a\nb");
        port.ready_input(&mut poller).expect("the read succeeds");
        port.flush_linebuf();
        assert_eq!(
            frames(&mut port),
            vec![(None, b"a\nb".to_vec())],
            "raw mode is untouched by the line flush"
        );
    }
    // -----------------------------------------------------------------------------------
    // The `driver_output2` header/body split (contract A.4)
    // -----------------------------------------------------------------------------------
    // The oracle pins one `{Port, {data, Data}}` message per driver `driver_output*` CALL
    // (`deliver_read_message`, `../../../erts/emulator/beam/io.c:3363-3442`): a header/body
    // pair is one message, two calls are two, and a `NULL` body (no binary tail at all) is
    // distinguishable from a present-but-empty one. This crate's half is the split carried on
    // [`ReadMessage`]; building `[H1,H2,H3|<<Body>>]` out of it is the runtime's.

    /// One scripted step a [`ReadScriptDriver`] performs in its single `ready_input`.
    enum ScriptStep {
        /// A `driver_output2(hbuf,hlen,buf,len)`-shaped read (a `NULL` `buf` is `None`).
        Split {
            header: &'static [u8],
            body: Option<&'static [u8]>,
        },
        /// A plain `driver_output(buf,len)`: bytes banked on [`DriverContext::input`].
        Bank(&'static [u8]),
    }

    /// A driver that replays a script of read hand-ups in one `ready_input`, then reports
    /// nothing more (so a second round is silent).
    struct ReadScriptDriver {
        steps: Vec<ScriptStep>,
        fed: bool,
    }

    impl Driver for ReadScriptDriver {
        fn name(&self) -> &str {
            "read-script-test"
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
            for step in &self.steps {
                match step {
                    ScriptStep::Split { header, body } => ctx.deliver_read(header, *body)?,
                    ScriptStep::Bank(bytes) => ctx.input().push(bytes),
                }
            }
            Ok(())
        }

        fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }
    }

    /// A port whose driver replays `steps` on its first `ready_input`.
    fn scripted_port(steps: Vec<ScriptStep>) -> (Poller, Port) {
        let poller = Poller::new().expect("a poll set");
        let port = Port::new(
            PortId::new(1),
            Box::new(ReadScriptDriver { steps, fed: false }),
        );
        (poller, port)
    }

    /// The exact `(header, body)` pairs a raw-mode port owes, in arrival order.
    fn reads(port: &mut Port) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        port.take_input_notifications()
            .into_iter()
            .map(|notification| match notification {
                PortNotification::Data { read, .. } => (read.header, read.body),
                other => panic!("the port reported an unexpected notification: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn one_split_read_is_one_notification_with_its_header_apart() {
        let (mut poller, mut port) = scripted_port(vec![ScriptStep::Split {
            header: b"HDR",
            body: Some(b"abc"),
        }]);
        port.ready_input(&mut poller).expect("the read succeeds");
        assert_eq!(
            reads(&mut port),
            vec![(b"HDR".to_vec(), Some(b"abc".to_vec()))],
            "header apart from body, never flattened into one buffer"
        );
        assert!(reads(&mut port).is_empty(), "reported once, never twice");
    }

    #[test]
    fn one_notification_per_driver_output_call_and_never_merged() {
        // The five rows of `ReadMessage`'s pinned table, in one callback: each call is its own
        // message, in call order, and the banked bytes are closed into a body-only read where
        // they were `push`ed — before the split read that followed them.
        let (mut poller, mut port) = scripted_port(vec![
            ScriptStep::Split {
                header: b"HDR",
                body: Some(b"abc"),
            },
            ScriptStep::Split {
                header: b"HDR",
                body: Some(b""),
            },
            ScriptStep::Split {
                header: b"HDR",
                body: None,
            },
            ScriptStep::Split {
                header: b"",
                body: Some(b"abc"),
            },
            ScriptStep::Bank(b"bank"),
            ScriptStep::Split {
                header: b"",
                body: None,
            },
        ]);
        port.ready_input(&mut poller).expect("the read succeeds");
        assert_eq!(
            reads(&mut port),
            vec![
                (b"HDR".to_vec(), Some(b"abc".to_vec())),
                // `driver_output2("HDR",3,"abc",0)`: a present, empty body (`[H|<<>>]`).
                (b"HDR".to_vec(), Some(Vec::new())),
                // `driver_output2("HDR",3,NULL,0)`: no binary tail at all (the proper list).
                (b"HDR".to_vec(), None),
                // `driver_output("abc",3)`: all body.
                (Vec::new(), Some(b"abc".to_vec())),
                // Bytes banked on `ctx.input()`, closed when the next split read arrived.
                (Vec::new(), Some(b"bank".to_vec())),
                // `driver_output2(NULL,0,NULL,0)`: a real, empty message.
                (Vec::new(), None),
            ],
            "one notification per call, arrival order kept, nothing merged"
        );
        assert!(
            reads(&mut port).is_empty(),
            "a second round re-reports nothing"
        );
    }

    #[test]
    fn banked_bytes_alone_are_closed_into_one_body_only_read() {
        // The plain `driver_output` shape with no explicit split read: the harvest closes the
        // open frame, so a `push`-only driver keeps working exactly as before.
        let (mut poller, mut port) = scripted_port(vec![
            ScriptStep::Bank(b"a"),
            ScriptStep::Bank(b"bc"),
        ]);
        port.ready_input(&mut poller).expect("the read succeeds");
        assert_eq!(
            reads(&mut port),
            vec![(Vec::new(), Some(b"abc".to_vec()))],
            "a bank is one read once the frame closes"
        );
    }

    #[test]
    fn the_input_byte_view_keeps_the_split_messages_in_arrival_order() {
        let (mut poller, mut port) = scripted_port(vec![
            ScriptStep::Split {
                header: b"HDR",
                body: Some(b"abc"),
            },
            ScriptStep::Bank(b"tail"),
        ]);
        port.ready_input(&mut poller).expect("the read succeeds");
        // The header bytes count towards the C's `bytes_in` (what `port_info(P,input)` shows),
        // and the open frame's accumulator counts too.
        assert_eq!(port.input_len(), 3 + 3 + 4);
        assert!(port.has_input());
        // A flat read sees everything in order, header first.
        let mut first = [0u8; 4];
        assert_eq!(port.read_input(&mut first), 4);
        assert_eq!(&first, b"HDRa");
        assert_eq!(port.input_len(), 6, "the consumed prefix is gone");
        assert_eq!(port.drain_input(), b"bctail".to_vec());
        assert_eq!(port.input_len(), 0);
        assert!(!port.has_input());
        assert_eq!(port.drain_input(), Vec::<u8>::new());
    }

    #[test]
    fn every_line_frame_carries_the_header_of_the_read_that_completed_it() {
        // `driver_output2("HDR",3,"a\nb\n",4)` ⇒ two messages, each with the header prefix:
        // `{eol,[72,68,82|<<"a">>]}` and `{eol,[72,68,82|<<"b">>]}`.
        let (mut poller, mut port) = scripted_port(vec![ScriptStep::Split {
            header: b"HDR",
            body: Some(b"a\nb\n"),
        }]);
        port.set_line_mode(Some(0));
        port.ready_input(&mut poller).expect("the read succeeds");
        assert!(
            port.take_input_notifications().is_empty(),
            "line mode reports through the line channel only"
        );
        let staged = port.take_line_frames();
        assert_eq!(staged.len(), 2, "one frame per completed line");
        assert_eq!(
            staged
                .iter()
                .map(|f| (f.header.clone(), f.bytes.clone(), f.eol))
                .collect::<Vec<_>>(),
            vec![
                (b"HDR".to_vec(), b"a".to_vec(), true),
                (b"HDR".to_vec(), b"b".to_vec(), true),
            ],
            "each frame carries the header of the read that produced it"
        );
    }

    #[test]
    fn a_line_spanning_two_reads_gets_the_header_of_the_read_that_completed_it() {
        let (mut poller, mut port) = scripted_port(vec![
            ScriptStep::Split {
                header: b"A",
                body: Some(b"par"),
            },
            ScriptStep::Split {
                header: b"B",
                body: Some(b"tial\n"),
            },
        ]);
        port.set_line_mode(Some(0));
        port.ready_input(&mut poller).expect("the read succeeds");
        assert!(port.take_input_notifications().is_empty());
        let staged = port.take_line_frames();
        assert_eq!(staged.len(), 1);
        assert_eq!(
            (staged[0].header.clone(), staged[0].bytes.clone()),
            (b"B".to_vec(), b"partial".to_vec()),
            "the completing read's header, not the first one's"
        );
    }

    #[test]
    fn a_null_body_stages_no_line_frame_and_the_eof_flush_has_no_header() {
        let (mut poller, mut port) = scripted_port(vec![
            // A NULL body produces no frame (there are no body bytes to split into lines), and
            // it must not disturb the partial line already held.
            ScriptStep::Split {
                header: b"HDR",
                body: None,
            },
            ScriptStep::Split {
                header: b"HDR",
                body: Some(b"par"),
            },
            ScriptStep::Split {
                header: b"HDR",
                body: None,
            },
        ]);
        port.set_line_mode(Some(0));
        port.ready_input(&mut poller).expect("the read succeeds");
        assert!(port.take_input_notifications().is_empty());
        assert!(
            port.take_line_frames().is_empty(),
            "a `NULL` body stages no frame; the partial line is still held"
        );
        // The end-of-file flush releases the partial line as `{noeol, "par"}` — with no header
        // of its own.
        port.flush_linebuf();
        let staged = port.take_line_frames();
        assert_eq!(
            staged
                .iter()
                .map(|f| (f.header.clone(), f.bytes.clone(), f.eol))
                .collect::<Vec<_>>(),
            vec![(Vec::new(), b"par".to_vec(), false)],
            "the flushed partial line carries an empty header"
        );
        port.flush_linebuf();
        assert!(
            port.take_line_frames().is_empty(),
            "a repeated flush stages nothing"
        );
    }
}
