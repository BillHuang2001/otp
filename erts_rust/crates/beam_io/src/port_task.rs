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

//! Per-port task scheduling: the Rust analogue of the ERTS port-task queue.
//!
//! The legacy C never calls a driver straight from the poll loop; it *schedules* a port task
//! ([`PortTaskKind`] — `ERTS_PORT_TASK_INPUT`/`OUTPUT`/`TIMEOUT`) on the port and lets a
//! scheduler pick the port up and run the task under a reduction budget
//! (`erts_port_task_schedule`/`erts_port_task_execute`, `erts/emulator/beam/erl_port_task.c`).
//! This module reproduces that shape:
//!
//! * [`PortTask`] — one unit of port work: a [`PortTaskKind`], an optional deadline for
//!   task kinds that have one, and the enqueue sequence number the task was stamped with;
//! * [`PortTaskQueue`] — the per-port queue: **at most one pending task per kind** (the C
//!   schedules an I/O direction only through that direction's own `ErtsIoTask` handle and a
//!   port keeps a single `timeout_task` handle, so one kind can be scheduled only once at a
//!   time), dequeued by priority `INPUT` → `OUTPUT` → `TIMEOUT` (the `ErtsPortTaskType`
//!   order in `erl_port_task.h`). A task with a deadline is only handed out once the
//!   deadline has passed, so a not-yet-due timer never runs early;
//! * [`Readiness`] — the poller-readiness bits an event carries, and the rule that decides
//!   which tasks it schedules (readability **or** read-EOF **or** an error schedules
//!   `INPUT`; writability **or** write-EOF **or** an error schedules `OUTPUT` — a poll
//!   error re-triggers *every* armed direction, see [`Readiness`]);
//! * [`PortNotification`] / [`PortTaskOutcome`] — what an execution round reports back to
//!   the runtime. This is the seam the runtime turns into `{Port, {data, Data}}` messages, and
//!   [`PortNotification::Data`] carries a whole [`ReadMessage`] so the C's `driver_output2`
//!   header/body split survives up to the term the runtime builds.
//!
//! # Reductions
//!
//! Running a task costs reductions, with the C's per-kind constants:
//! `ERTS_PORT_REDS_INPUT`/`ERTS_PORT_REDS_OUTPUT`/`ERTS_PORT_REDS_TIMEOUT` are
//! `CONTEXT_REDS/100` (see [`PORT_REDS_INPUT`]) and a port execution round is handed
//! `ERTS_PORT_REDS_EXECUTE` = `CONTEXT_REDS/4` (see [`PORT_REDS_EXECUTE`]) —
//! `erts/emulator/beam/erl_port.h`. [`crate::PortRegistry::execute_port_tasks`] stops as
//! soon as the caller's budget is exhausted and leaves the remaining tasks queued, and
//! [`PortTaskOutcome::reds_used`] is the sum of the per-task [`PortTaskKind::reductions`].
//!
//! That accounting is **not** the whole of the C's. `erts_port_task_execute` charges a task
//! `ERTS_PORT_REDS_{INPUT,OUTPUT,TIMEOUT}` **plus** the extra reductions
//! `erts_port_driver_callback_epilogue` returns (`ERTS_PORT_REDS_TERMINATE` when a closing
//! port's I/O queue has drained, plus the cost of cutting that port's cross-port links,
//! `erts/emulator/beam/erl_port.h:799-824`) **plus** `ERTS_PORT_CALLBACK_VREDS`
//! (`CONTEXT_REDS/20`), adds `ERTS_PORT_REDS_FREE` when the port is being freed, and bounds
//! the round at `CONTEXT_REDS` — the port's reds are *initialized* to
//! `ERTS_PORT_REDS_EXECUTE` and the loop breaks once `pp->reds >= CONTEXT_REDS`
//! (`erts/emulator/beam/erl_port_task.c:1730,1833,1848,1854-1860`). None of those extra
//! charges (nor the `ERTS_PORT_REDS_FREE` teardown charge) is modelled here.
//!
//! # Ordering — what is matched, and what is not
//!
//! The C keeps *three* queues per port (a local queue, a busy-caller queue and the poll
//! thread's inbound queue) and processes them FIFO; it has no task-type priority. This crate
//! uses a single queue ordered by task **kind priority** (`INPUT` before `OUTPUT` before
//! `TIMEOUT`, the enum order of `ErtsPortTaskType`). That is a documented Milestone-1
//! simplification of the C's queue split; the only observable difference is the order two
//! different kinds are run in when both are scheduled within one round.
//!
//! Because the queue holds **at most one pending task per kind** and the three kinds have
//! distinct priorities, [`PortTask::seq`] can never break a tie: the enqueue sequence is
//! recorded and exposed, but the ordering is decided by the kind alone.
//!
//! The **deadline gate** is a second divergence. The C schedules a port's `TIMEOUT` task only
//! once its timer has actually expired (`port_timeout_common`,
//! `erts/emulator/beam/erl_hl_timer.c`), so the `ERTS_PTMR_IS_TIMED_OUT` test inside
//! `erts_port_task_execute` is a race guard: a `TIMEOUT` task that is *not* yet timed out is
//! **dequeued, charged 0 reductions and dropped without calling the driver's `timeout`**
//! (`erts/emulator/beam/erl_port_task.c:1759-1774`). This crate instead arms the `TIMEOUT`
//! task with its deadline up front and [`PortTaskQueue::pop_next`] answers `None` while the
//! deadline has not passed, so the task **stays queued** and still reports through
//! [`PortTaskQueue::next`]/[`PortTaskQueue::contains`]/[`PortTaskQueue::len`]. Neither design
//! runs the callback early; they differ in what becomes of the not-yet-due task.

use std::time::Instant;

use beam_process::PortId;

use crate::poller::Ready;

/// The number of reductions a scheduler runs before it swaps the entity it is executing
/// out (`CONTEXT_REDS`, `erts/emulator/beam/erl_vm.h`).
///
/// This is the canonical [`beam_common::CONTEXT_REDS`], re-exported here so the existing
/// `beam_io::port_task::CONTEXT_REDS` / `beam_io::CONTEXT_REDS` paths keep resolving; the
/// definition lives in `beam_common`, there is no local copy.
pub use beam_common::CONTEXT_REDS;

/// The reductions one `INPUT` port task costs (`ERTS_PORT_REDS_INPUT` = `CONTEXT_REDS/100`).
pub const PORT_REDS_INPUT: usize = CONTEXT_REDS / 100;

/// The reductions one `OUTPUT` port task costs (`ERTS_PORT_REDS_OUTPUT` = `CONTEXT_REDS/100`).
pub const PORT_REDS_OUTPUT: usize = CONTEXT_REDS / 100;

/// The reductions one `TIMEOUT` port task costs (`ERTS_PORT_REDS_TIMEOUT` = `CONTEXT_REDS/100`).
pub const PORT_REDS_TIMEOUT: usize = CONTEXT_REDS / 100;

/// The reduction budget one whole port execution round gets (`ERTS_PORT_REDS_EXECUTE` =
/// `CONTEXT_REDS/4`).
pub const PORT_REDS_EXECUTE: usize = CONTEXT_REDS / 4;

/// The kind of work a scheduled port task represents (`ErtsPortTaskType`).
///
/// The declaration order is the scheduling **priority** ([`PortTaskKind::priority`]):
/// `Input` (0) before `Output` (1) before `Timeout` (2), matching the `ErtsPortTaskType`
/// values in `erts/emulator/beam/erl_port_task.h`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum PortTaskKind {
    /// Read readiness: run the driver's `ready_input`.
    Input,
    /// Write readiness (or queued command data): run the driver's `ready_output`.
    Output,
    /// A port timer expired: run the driver's `timeout`.
    Timeout,
}

impl PortTaskKind {
    /// This kind's scheduling priority (lower runs first).
    pub const fn priority(self) -> u8 {
        match self {
            PortTaskKind::Input => 0,
            PortTaskKind::Output => 1,
            PortTaskKind::Timeout => 2,
        }
    }

    /// The reductions running a task of this kind costs.
    pub const fn reductions(self) -> usize {
        match self {
            PortTaskKind::Input => PORT_REDS_INPUT,
            PortTaskKind::Output => PORT_REDS_OUTPUT,
            PortTaskKind::Timeout => PORT_REDS_TIMEOUT,
        }
    }
}

/// One scheduled unit of port work.
///
/// A task carries its [`PortTaskKind`], an optional `deadline` (only set for
/// [`PortTaskKind::Timeout`] tasks) and the `seq` it was enqueued with — its enqueue order;
/// see [`PortTaskQueue`] for why the queue's ordering never consults it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PortTask {
    /// What the task does when it runs.
    kind: PortTaskKind,
    /// When the task may run; `None` means "as soon as possible".
    deadline: Option<Instant>,
    /// The enqueue sequence number.
    seq: u64,
}

impl PortTask {
    /// This task's kind.
    pub fn kind(&self) -> PortTaskKind {
        self.kind
    }

    /// This task's deadline, if it has one.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// The sequence number this task was enqueued with.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The reductions running this task costs.
    pub fn reductions(&self) -> usize {
        self.kind.reductions()
    }

    /// Whether the task may run at `now` (no deadline, or the deadline has passed).
    pub fn is_ready_at(&self, now: Instant) -> bool {
        match self.deadline {
            Some(deadline) => deadline <= now,
            None => true,
        }
    }
}

/// A per-port task queue: one pending task per [`PortTaskKind`], ordered by kind priority
/// (the enqueue sequence is recorded, but cannot break a tie — see the module docs).
///
/// Enqueueing a kind that is already pending is a no-op (it returns `false`), which is what
/// keeps the queue from growing without bound when a descriptor stays ready across poll
/// rounds: the pending task drains the descriptor to `EAGAIN`, so no wakeup is lost.
#[derive(Clone, Debug, Default)]
pub struct PortTaskQueue {
    /// The pending tasks, in enqueue order.
    tasks: Vec<PortTask>,
    /// The sequence number the next enqueued task gets.
    next_seq: u64,
}

impl PortTaskQueue {
    /// An empty queue.
    pub fn new() -> PortTaskQueue {
        PortTaskQueue {
            tasks: Vec::new(),
            next_seq: 0,
        }
    }

    /// Enqueues a task of `kind`, reporting whether it was added.
    ///
    /// Returns `false` (and leaves the queue unchanged) when a task of this kind is already
    /// pending.
    pub fn enqueue(&mut self, kind: PortTaskKind) -> bool {
        self.push(PortTask {
            kind,
            deadline: None,
            seq: u64::MAX,
        })
    }

    /// Enqueues a [`PortTaskKind::Timeout`] task that may not run before `deadline`,
    /// reporting whether it was added.
    pub fn enqueue_timeout(&mut self, deadline: Instant) -> bool {
        self.push(PortTask {
            kind: PortTaskKind::Timeout,
            deadline: Some(deadline),
            seq: u64::MAX,
        })
    }

    /// Arms the single [`PortTaskKind::Timeout`] task at `deadline`, **replacing** any
    /// deadline already armed for it.
    ///
    /// Unlike [`PortTaskQueue::enqueue_timeout`] — a no-op when a timer is already pending —
    /// this re-points the pending timer, which is what re-arming from a driver's *declared*
    /// deadline ([`crate::Driver::next_timeout`]) needs: the driver's latest declaration wins
    /// rather than being dropped because an older timer is still queued.
    pub fn arm_timeout(&mut self, deadline: Instant) {
        self.tasks.retain(|task| task.kind != PortTaskKind::Timeout);
        let _armed = self.push(PortTask {
            kind: PortTaskKind::Timeout,
            deadline: Some(deadline),
            seq: u64::MAX,
        });
    }

    /// The shared enqueue path: dedupe by kind, then stamp the sequence number.
    fn push(&mut self, mut task: PortTask) -> bool {
        if self.contains(task.kind) {
            return false;
        }
        task.seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.tasks.push(task);
        true
    }

    /// The number of pending tasks (0 or 1 per kind).
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether no task is pending.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Whether a task of `kind` is pending.
    pub fn contains(&self, kind: PortTaskKind) -> bool {
        self.tasks.iter().any(|task| task.kind == kind)
    }

    /// The kind of the task that would run next, regardless of any deadline.
    pub fn next(&self) -> Option<PortTaskKind> {
        self.highest_priority_index().map(|i| self.tasks[i].kind)
    }

    /// Removes and returns the highest-priority task that may run at `now`.
    ///
    /// Returns `None` when the queue is empty or when its highest-priority task is a
    /// [`PortTaskKind::Timeout`] whose deadline has not passed yet (that task stays queued).
    pub fn pop_next(&mut self, now: Instant) -> Option<PortTask> {
        let index = self.highest_priority_index()?;
        if !self.tasks[index].is_ready_at(now) {
            return None;
        }
        Some(self.tasks.remove(index))
    }

    /// Removes a pending task of `kind`, reporting whether one was pending.
    pub fn remove(&mut self, kind: PortTaskKind) -> bool {
        match self.tasks.iter().position(|task| task.kind == kind) {
            Some(index) => {
                self.tasks.remove(index);
                true
            }
            None => false,
        }
    }

    /// Drops every pending task.
    pub fn clear(&mut self) {
        self.tasks.clear();
    }

    /// The index of the pending task that runs first: lowest kind priority. The enqueue
    /// sequence (`task.seq`) can never break this tie, because no two pending tasks share a
    /// kind and hence a priority.
    fn highest_priority_index(&self) -> Option<usize> {
        self.tasks
            .iter()
            .enumerate()
            .min_by_key(|(_, task)| (task.kind.priority(), task.seq))
            .map(|(index, _)| index)
    }
}

/// The subset of a poller [`Ready`] mask that decides which port tasks an event schedules.
///
/// [`Readiness`] is deliberately a plain, constructible value (unlike [`Ready`], whose
/// fields the poller owns) so the reactor's scheduling rule can be exercised directly:
/// `read_ready` is readability **or** a read half-close **or** an error, and `write_ready`
/// is writability **or** a write half-close **or** an error.
///
/// The `error` bit reaching **both** directions mirrors the legacy reactor. `EPOLLERR` and
/// `EPOLLHUP` are one mask there (`ERTS_POLL_EV_ERR`, `erts/emulator/sys/common/erl_poll.h:184`),
/// and a poll error is answered by re-triggering **every** event that has been selected — `revents
/// = state->active_events` — after which both the out and the in callback fire
/// (`erts/emulator/sys/common/erl_check_io.c:2018-2027`, `:2099-2110`). So a pure `EPOLLERR`
/// without `EPOLLHUP` schedules `INPUT` *and* `OUTPUT`, not just `INPUT`. The armed-direction
/// gate still applies (see [`crate::PortRegistry::schedule_readiness`]), so a port that never
/// armed a write fd still gets no output task.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Readiness {
    /// The descriptor reported readability.
    pub readable: bool,
    /// The descriptor reported writability.
    pub writable: bool,
    /// The descriptor reported an error condition.
    pub error: bool,
    /// The peer closed its end: reads now yield end-of-file.
    pub read_closed: bool,
    /// The peer stopped accepting writes.
    pub write_closed: bool,
}

impl Readiness {
    /// Whether the mask schedules an [`PortTaskKind::Input`] task.
    ///
    /// A pure EOF/`EPOLLHUP` event (`read_closed` with `readable == false`) must still reach
    /// the driver's `ready_input`, which is where the end-of-file latch lives.
    pub fn read_ready(self) -> bool {
        self.readable || self.read_closed || self.error
    }

    /// Whether the mask schedules an [`PortTaskKind::Output`] task.
    ///
    /// `error` is included for the same reason [`Readiness::read_ready`] includes it: the C
    /// reactor re-triggers *every* armed direction on a poll error (`ERTS_POLL_EV_ERR` is
    /// `EPOLLERR | EPOLLHUP`, `erl_poll.h:184`; `revents = state->active_events`,
    /// `erl_check_io.c:2018-2027`), so a pure error also reaches the driver's `ready_output`.
    /// Whether an `Output` task is actually scheduled is still gated on the driver having
    /// armed a write fd (see [`crate::PortRegistry::schedule_readiness`]).
    pub fn write_ready(self) -> bool {
        self.writable || self.write_closed || self.error
    }

    /// Whether the mask schedules nothing at all.
    pub fn is_idle(self) -> bool {
        !self.read_ready() && !self.write_ready()
    }
}

impl From<Ready> for Readiness {
    fn from(ready: Ready) -> Readiness {
        Readiness {
            readable: ready.readable(),
            writable: ready.writable(),
            error: ready.is_error(),
            read_closed: ready.is_read_closed(),
            write_closed: ready.is_write_closed(),
        }
    }
}

/// One completed read a driver handed up, with the C's header/body split preserved.
///
/// This is the payload of one `driver_output*` **call**, not one byte chunk: the C delivers
/// exactly one `{Port, {data, Data}}` message per call (`deliver_read_message`,
/// `erts/emulator/beam/io.c:3363-3442`), and two calls made inside one driver callback are two
/// messages. [`header`](ReadMessage::header) is the `hbuf`/`hlen` pair of
/// `driver_output2`/`driver_output_binary`/`driver_outputv`; [`body`](ReadMessage::body) is
/// its `buf`/`len` pair, with the C's `buf == NULL` distinction kept as `None`.
///
/// The runtime renders the pair into the term the owner sees — this crate only carries the
/// split, it never builds the term:
///
/// * `[list]` port: the header bytes then the body bytes, **flat**, as a proper int list (the
///   split is not observable at all: `driver_output2("HDR",3,"abc",3)` ⇒ `[72,68,82,97,98,99]`).
/// * `[binary]` port: the header bytes as **one cons cell per byte** (never one nested binary)
///   and the body as the improper list's binary tail. A non-empty header with
///   `body == Some(b)` is `[72,68,82\|<<b>>]`; with `body == None` it is the **proper** list
///   `[72,68,82]` (the C adds no binary tail at all for a `NULL` body); with an **empty**
///   header the body is the bare binary (`driver_output("abc",3)` ⇒ `<<97,98,99>>`), and an
///   empty header with no body is `[]`.
///
/// The pinned oracle shapes (system `erl`, OTP 29.1.1/erts-17.1, probed with a real C driver):
///
/// | driver call | `[list]` port `Data` | `[binary]` port `Data` |
/// |---|---|---|
/// | `driver_output2("HDR",3,"abc",3)` | `[72,68,82,97,98,99]` | `[72,68,82\|<<97,98,99>>]` (improper: `is_list` true, `length/1` `badarg`) |
/// | `driver_output("abc",3)` | `[97,98,99]` | `<<97,98,99>>` (bare binary) |
/// | `driver_output2("HDR",3,"abc",0)` | `[72,68,82]` | `[72,68,82\|<<>>]` (empty binary tail) |
/// | `driver_output2("HDR",3,"",0)` — non-NULL body, length 0 | `[72,68,82]` | `[72,68,82\|<<>>]` |
/// | `driver_output2("HDR",3,NULL,0)` — **NULL** body | `[72,68,82]` | `[72,68,82]` (proper list) |
/// | `driver_output2(NULL,0,NULL,0)` | `[]` | `[]` (not `<<>>`) |
///
/// So `body: Some(vec![])` and `body: None` are **different** messages, which is why the body
/// is an `Option` rather than an always-present `Vec`. A header-only read is
/// `body: None`; [`ReadMessage::body_only`] builds the common `driver_output` shape.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ReadMessage {
    /// The `driver_output2` `hbuf`/`hlen` bytes — delivered as one cons cell per byte in
    /// binary mode and as plain ints in list mode, never as one nested binary.
    pub header: Vec<u8>,
    /// The `driver_output2` `buf`/`len` bytes. `None` is the C's `buf == NULL`: **no** binary
    /// tail at all (a header-only proper list), whereas `Some(vec![])` is a present, empty body
    /// (`<<>>` tail ⇒ `[H…|<<>>]`).
    pub body: Option<Vec<u8>>,
}

impl ReadMessage {
    /// The [`ReadMessage`] of a plain `driver_output(bytes, len)` — all body, no header.
    pub fn body_only(bytes: Vec<u8>) -> ReadMessage {
        ReadMessage {
            header: Vec::new(),
            body: Some(bytes),
        }
    }

    /// The number of bytes this read carries: the C's `bytes_in` accounting for the call
    /// (`hlen + len`). This is what `port_info(P, input)` counts, header included.
    pub fn len(&self) -> usize {
        self.header.len() + self.body.as_ref().map_or(0, Vec::len)
    }

    /// Whether this read carries no bytes at all (`driver_output2(NULL,0,NULL,0)`, whose
    /// message is the still-owed `{Port, {data, []}}`).
    pub fn is_empty(&self) -> bool {
        self.header.is_empty() && self.body.as_ref().is_none_or(Vec::is_empty)
    }

    /// Whether the read has a **body** at all — the flag that decides the binary tail: `true`
    /// (`Some`, empty or not) ⇒ the tail exists, `false` (`None`) ⇒ there is none.
    pub fn has_body(&self) -> bool {
        self.body.is_some()
    }

    /// The read's bytes in one flat sequence: the header bytes, then the body bytes.
    ///
    /// This is exactly what an `[list]` port's `Data` is — the split is not observable there
    /// (`driver_output2("HDR",3,"abc",3)` ⇒ `[72,68,82,97,98,99]`) — and it is the byte stream
    /// a consumer that does not build a term (an `{packet, N}` framer, a byte counter) wants.
    /// A `[binary]` port must **not** use it to build its term: there the header is a cons
    /// prefix and the body the improper tail, so flattening loses the very distinction the C
    /// makes.
    pub fn flattened(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.len());
        bytes.extend_from_slice(&self.header);
        if let Some(body) = &self.body {
            bytes.extend_from_slice(body);
        }
        bytes
    }}

/// Something a port produced during an execution round, for its owner.
///
/// This is the crate's notification seam: the runtime turns [`PortNotification::Data`] into
/// the `{Port, {data, Data}}` message Erlang sees and [`PortNotification::Closed`] into a
/// `ERL_PORT_EXIT`-style port exit.
///
/// Every [`PortNotification::Data`] is **one driver `driver_output*` call** — a round reports
/// them in arrival order and there can be several for one port (a driver that answers two
/// commands in one callback produces two), and two calls are never merged. An explicit empty
/// read is a real notification: the oracle's `{Port, {data, []}}`.
///
/// A `{line, N}` port's input does **not** ride this enum: it is reported on the separate
/// [`LineNotification`] channel (see there), so this enum keeps exactly its two variants and
/// an exhaustive consumer keeps compiling unchanged.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PortNotification {
    /// The port's driver read `read`, which belongs to the port's owner.
    Data {
        /// The port that produced the bytes.
        port: PortId,
        /// The call's bytes, split exactly as the driver handed them up (see [`ReadMessage`]
        /// for the shapes, including the empty one).
        read: ReadMessage,
    },
    /// The port was closed since the previous execution round.
    Closed {
        /// The port that closed.
        port: PortId,
    },
}
/// One **line-framed** chunk a `{line, N}` port owes its owner.
///
/// This is the `{line, N}` mode (`ERTS_PORT_SFLG_LINEBUF_IO`): the port splits the bytes its
/// driver handed up into lines and wraps each one as the `{eol, Data}` / `{noeol, Data}`
/// term of `deliver_linebuf_message` (`erts/emulator/beam/io.c:3454`; the wrapper itself is
/// built at `io.c:3430-3435`). The bytes here are the frame's content — a consumer renders
/// them as `{Port, {data, {eol, Bytes}}}` / `{Port, {data, {noeol, Bytes}}}`.
///
/// This is deliberately a **separate struct**, not a third [`PortNotification`] variant: a
/// line-mode port's input never reaches the raw `{Port, {data, Data}}` channel, and a
/// consumer that matches [`PortNotification`] exhaustively stays exhaustive.
///
/// Unlike [`PortNotification::Data`], an empty payload is a **real** frame here: a blank line
/// is the oracle's `{Port, {data, {eol, []}}}`.
///
/// A frame carries the [`ReadMessage::header`] of the read that produced it in
/// [`header`](LineNotification::header), because the C prepends the header of **that call** as
/// cons cells to every frame the call's body completes
/// (`driver_output2("HDR",3,"a\nb\n",4)` ⇒ two messages, `{eol,[72,68,82|<<"a">>]}` and
/// `{eol,[72,68,82|<<"b">>]}`). A line that spans two calls therefore gets the header of the
/// call that **completed** it, a `NULL` body produces no frame at all, and a plain (header-less)
/// read — or the end-of-file flush — stamps an empty header.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LineNotification {
    /// The port that produced the frame.
    pub port: PortId,
    /// The per-frame header prefix: the producing read's [`ReadMessage::header`], added as one
    /// cons cell per byte ahead of the frame's content in binary mode. Empty for a plain read
    /// and for an end-of-file flush.
    pub header: Vec<u8>,
    /// The frame's content bytes (the line's terminator is not included); empty for a blank
    /// line.
    pub bytes: Vec<u8>,
    /// Whether the frame was terminated by a newline: `true` is the Erlang `eol` wrapper,
    /// `false` the `noeol` one (a length-limited chunk, or the partial line flushed at
    /// end-of-file).
    pub eol: bool,
}
/// What one [`crate::PortRegistry::execute_port_tasks`] round did.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PortTaskOutcome {
    /// How many port tasks ran.
    pub ran: usize,
    /// How many reductions those tasks cost: the sum of the per-task
    /// [`PortTaskKind::reductions`] (see the module docs on what the C charges on top).
    pub reds_used: usize,
    /// The notifications the round produced (see [`PortNotification`]).
    pub notifications: Vec<PortNotification>,
}

#[cfg(test)]
mod tests {
    use super::{
        PortTaskKind, PortTaskQueue, ReadMessage, Readiness, CONTEXT_REDS, PORT_REDS_EXECUTE,
        PORT_REDS_INPUT, PORT_REDS_OUTPUT, PORT_REDS_TIMEOUT,
    };
    use crate::driver::{Driver, DriverContext};
    use crate::drivers::FdDriver;
    use crate::poller::{Interest, Poller, Ready, Token};
    use crate::PortRegistry;
    use beam_common::Result;
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    #[test]
    fn body_only_is_the_plain_driver_output_shape() {
        let read = ReadMessage::body_only(b"abc".to_vec());
        assert!(read.header.is_empty(), "no header: never one nested binary");
        assert_eq!(read.body.as_deref(), Some(&b"abc"[..]));
        assert_eq!(read.len(), 3);
        assert!(!read.is_empty());
        assert!(read.has_body(), "a body-only read still has its binary tail");
    }

    #[test]
    fn len_is_the_cs_bytes_in_accounting_header_included() {
        // `bytes_in` (`io.c:3363-3442`) counts `hlen + len`, which is what
        // `port_info(P, input)` reports.
        let split = ReadMessage {
            header: b"HDR".to_vec(),
            body: Some(b"abc".to_vec()),
        };
        assert_eq!(split.len(), 6, "hlen + len");
        let header_only = ReadMessage {
            header: b"HDR".to_vec(),
            body: None,
        };
        assert_eq!(header_only.len(), 3, "a NULL body contributes nothing");
        let empty_body = ReadMessage {
            header: b"HDR".to_vec(),
            body: Some(Vec::new()),
        };
        assert_eq!(empty_body.len(), 3, "a present but empty body adds no bytes");
        assert_eq!(ReadMessage::default().len(), 0);
    }

    #[test]
    fn an_empty_body_differs_from_a_null_body() {
        // `driver_output2("HDR",3,"abc",0)` ⇒ `[72,68,82|<<>>]`; the same call with a NULL
        // body ⇒ the *proper* list `[72,68,82]`. Both carry 3 bytes, so only `has_body`
        // distinguishes them — it is the flag that decides whether the binary tail exists.
        let empty = ReadMessage {
            header: b"HDR".to_vec(),
            body: Some(Vec::new()),
        };
        let null = ReadMessage {
            header: b"HDR".to_vec(),
            body: None,
        };
        assert_eq!(empty.len(), null.len());
        assert!(empty.has_body() && !null.has_body());
        assert!(
            !empty.is_empty() && !null.is_empty(),
            "the 3 header bytes are bytes: neither message is empty"
        );
        // …whereas the body-less, header-less read really carries nothing.
        assert!(ReadMessage::default().is_empty());
    }

    #[test]
    fn a_null_read_is_empty_but_still_a_reportable_message() {
        // `driver_output2(NULL,0,NULL,0)` is the oracle's `{Port, {data, []}}`: no bytes at
        // all, yet still one message owed to the owner.
        let empty = ReadMessage::default();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(empty.header.is_empty() && !empty.has_body());
    }

    #[test]
    fn flattened_is_the_list_mode_byte_sequence() {
        // The `[list]` port's `Data` is header then body, flat — the split is invisible there.
        let split = ReadMessage {
            header: b"HDR".to_vec(),
            body: Some(b"abc".to_vec()),
        };
        assert_eq!(split.flattened(), b"HDRabc".to_vec());
        assert_eq!(
            ReadMessage::body_only(b"abc".to_vec()).flattened(),
            b"abc".to_vec()
        );
        assert_eq!(
            ReadMessage {
                header: b"HDR".to_vec(),
                body: None,
            }
            .flattened(),
            b"HDR".to_vec()
        );
        assert!(ReadMessage::default().flattened().is_empty());
    }
    #[test]
    fn the_pinned_oracle_rows_map_onto_the_header_body_split() {
        // One [`ReadMessage`] per `driver_output*` call, in the order the header table in
        // `ReadMessage`'s docs lists them: the split (not the rendering) is this crate's
        // half, so the rows are pinned as `(header, body, has_body, len)`.
        let rows = [
            // driver_output2("HDR",3,"abc",3)
            (
                ReadMessage {
                    header: b"HDR".to_vec(),
                    body: Some(b"abc".to_vec()),
                },
                true,
                6,
            ),
            // driver_output("abc",3)
            (ReadMessage::body_only(b"abc".to_vec()), true, 3),
            // driver_output2("HDR",3,"abc",0) — non-NULL, empty body
            (
                ReadMessage {
                    header: b"HDR".to_vec(),
                    body: Some(Vec::new()),
                },
                true,
                3,
            ),
            // driver_output2("HDR",3,NULL,0) — NULL body: no binary tail
            (
                ReadMessage {
                    header: b"HDR".to_vec(),
                    body: None,
                },
                false,
                3,
            ),
            // driver_output2(NULL,0,NULL,0)
            (ReadMessage::default(), false, 0),
        ];
        for (read, has_body, len) in rows {
            assert_eq!(read.has_body(), has_body, "{read:?}");
            assert_eq!(read.len(), len, "{read:?}");
        }
    }    /// A driver that arms **both** poll directions on one borrowed descriptor.
    ///
    /// [`FdDriver::pipe`] arms only `READABLE` (it registers its write fd only once there is
    /// something to flush), so it cannot express "a port whose write direction is armed" — the
    /// case a poll error must reach in both directions. This driver registers the descriptor
    /// with [`Interest::BOTH`] and deregisters it on `stop`; the owning [`UnixStream`] owns
    /// the fd, so the driver never closes it.
    struct BothDirectionsDriver {
        fd: RawFd,
        token: Option<Token>,
    }

    impl Driver for BothDirectionsDriver {
        fn name(&self) -> &str {
            "both-directions-test"
        }

        fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
            self.token = Some(ctx.register(self.fd, Interest::BOTH)?);
            Ok(())
        }

        fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
            Ok(0)
        }

        fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
            if let Some(token) = self.token.take() {
                ctx.deregister(self.fd, token)?;
            }
            Ok(())
        }
    }

    #[test]
    fn reduction_costs_match_the_legacy_constants() {
        assert_eq!(CONTEXT_REDS, 4000);
        assert_eq!(PORT_REDS_INPUT, 40);
        assert_eq!(PORT_REDS_OUTPUT, 40);
        assert_eq!(PORT_REDS_TIMEOUT, 40);
        assert_eq!(PORT_REDS_EXECUTE, 1000);
        assert_eq!(PortTaskKind::Input.reductions(), PORT_REDS_INPUT);
        assert_eq!(PortTaskKind::Output.reductions(), PORT_REDS_OUTPUT);
        assert_eq!(PortTaskKind::Timeout.reductions(), PORT_REDS_TIMEOUT);
    }

    #[test]
    fn kind_priority_follows_the_legacy_enum_order() {
        assert!(PortTaskKind::Input.priority() < PortTaskKind::Output.priority());
        assert!(PortTaskKind::Output.priority() < PortTaskKind::Timeout.priority());
        assert!(PortTaskKind::Input < PortTaskKind::Output);
        assert!(PortTaskKind::Output < PortTaskKind::Timeout);
    }

    #[test]
    fn a_fresh_queue_is_empty() {
        let queue = PortTaskQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.next(), None);
        assert!(!queue.contains(PortTaskKind::Input));
    }

    #[test]
    fn enqueue_dedupes_one_task_of_a_kind() {
        let mut queue = PortTaskQueue::new();
        assert!(queue.enqueue(PortTaskKind::Input));
        assert!(!queue.enqueue(PortTaskKind::Input), "already pending");
        assert_eq!(queue.len(), 1);
        assert!(queue.contains(PortTaskKind::Input));

        assert!(queue.enqueue(PortTaskKind::Output));
        assert_eq!(queue.len(), 2, "a different kind is a different task");

        let now = Instant::now();
        assert_eq!(
            queue.pop_next(now).expect("input runs first").kind(),
            PortTaskKind::Input
        );
        assert_eq!(
            queue.pop_next(now).expect("output next").kind(),
            PortTaskKind::Output
        );
        assert!(queue.pop_next(now).is_none());
        assert!(queue.enqueue(PortTaskKind::Input), "re-arming works");
    }

    #[test]
    fn dequeue_order_is_priority_then_fifo() {
        let mut queue = PortTaskQueue::new();
        // Enqueue in reverse priority order; the sequence numbers follow the calls.
        assert!(queue.enqueue(PortTaskKind::Timeout));
        assert!(queue.enqueue(PortTaskKind::Output));
        assert!(queue.enqueue(PortTaskKind::Input));
        assert_eq!(queue.len(), 3);

        let now = Instant::now();
        let order: Vec<PortTaskKind> = std::iter::from_fn(|| queue.pop_next(now))
            .map(|task| task.kind())
            .collect();
        assert_eq!(
            order,
            vec![
                PortTaskKind::Input,
                PortTaskKind::Output,
                PortTaskKind::Timeout
            ]
        );
    }

    #[test]
    fn sequence_numbers_are_monotonic_and_stable() {
        let mut queue = PortTaskQueue::new();
        assert!(queue.enqueue(PortTaskKind::Output));
        assert!(queue.enqueue(PortTaskKind::Input));
        let now = Instant::now();
        let input = queue.pop_next(now).expect("input first");
        let output = queue.pop_next(now).expect("output second");
        assert_eq!(input.seq(), 1, "the input task was enqueued second");
        assert_eq!(output.seq(), 0, "the output task was enqueued first");
        assert_eq!(
            input.reductions(),
            PORT_REDS_INPUT,
            "the task knows its cost"
        );
    }

    #[test]
    fn the_sequence_number_can_never_break_a_tie() {
        // The queue holds at most one pending task per kind, and the three kinds have
        // distinct priorities, so `highest_priority_index`'s `min_by_key((priority, seq))`
        // never reaches the `seq` component: the ordering is decided by the kind alone. The
        // `Timeout` task below is enqueued *before* the `Input` task (so it has the lower
        // sequence number) yet runs last, and re-enqueueing a pending kind is a no-op, so no
        // second same-priority task can ever exist.
        let mut queue = PortTaskQueue::new();
        assert!(queue.enqueue(PortTaskKind::Output));
        assert!(queue.enqueue(PortTaskKind::Timeout));
        assert!(queue.enqueue(PortTaskKind::Input));
        assert!(!queue.enqueue(PortTaskKind::Output), "no second Output task");
        assert!(!queue.enqueue(PortTaskKind::Timeout), "no second Timeout task");
        assert!(!queue.enqueue(PortTaskKind::Input), "no second Input task");
        assert_eq!(queue.len(), 3, "one task per kind, however often enqueued");

        let now = Instant::now();
        let popped: Vec<(PortTaskKind, u64)> = std::iter::from_fn(|| queue.pop_next(now))
            .map(|task| (task.kind(), task.seq()))
            .collect();
        assert_eq!(
            popped,
            vec![
                (PortTaskKind::Input, 2),
                (PortTaskKind::Output, 0),
                (PortTaskKind::Timeout, 1)
            ],
            "the kind priority decides: the *highest* sequence number runs first"
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn a_not_yet_due_timer_stays_queued() {
        // The documented divergence from `erts_port_task_execute`: the C dequeues a `TIMEOUT`
        // task that is not yet timed out, charges it 0 reductions and drops it
        // (`erl_port_task.c:1759-1774`). Here the task stays pending — nothing runs either
        // way, so no reductions are charged for a withheld timer.
        let mut queue = PortTaskQueue::new();
        let now = Instant::now();
        let deadline = now + Duration::from_secs(5);
        assert!(queue.enqueue_timeout(deadline));

        assert!(queue.pop_next(now).is_none(), "nothing may run yet");
        assert_eq!(queue.len(), 1, "the timer is still pending, not consumed");
        assert!(queue.contains(PortTaskKind::Timeout));
        assert_eq!(queue.next(), Some(PortTaskKind::Timeout));
    }

    #[test]
    fn a_timeout_task_is_not_handed_out_before_its_deadline() {
        let mut queue = PortTaskQueue::new();
        let now = Instant::now();
        let deadline = now + Duration::from_secs(30);
        assert!(queue.enqueue_timeout(deadline));
        assert_eq!(queue.next(), Some(PortTaskKind::Timeout));

        // Not due yet: the round must stop rather than run the timer early.
        assert!(queue.pop_next(now).is_none(), "not due yet");
        assert_eq!(queue.next(), Some(PortTaskKind::Timeout));

        let later = deadline + Duration::from_millis(1);
        let task = queue.pop_next(later).expect("now it is due");
        assert_eq!(task.kind(), PortTaskKind::Timeout);
        assert_eq!(task.deadline(), Some(deadline));
        assert_eq!(task.reductions(), PORT_REDS_TIMEOUT);
        assert!(queue.is_empty());
    }

    #[test]
    fn a_due_input_task_is_not_blocked_by_a_pending_timer() {
        let mut queue = PortTaskQueue::new();
        let now = Instant::now();
        assert!(queue.enqueue_timeout(now + Duration::from_secs(60)));
        assert!(queue.enqueue(PortTaskKind::Input));
        // Input has the higher priority, so the unexpired timer does not stall it.
        assert_eq!(
            queue.pop_next(now).expect("input runs").kind(),
            PortTaskKind::Input
        );
        assert!(queue.contains(PortTaskKind::Timeout), "the timer stays queued");
    }

    #[test]
    fn arm_timeout_replaces_a_pending_deadline() {
        let mut queue = PortTaskQueue::new();
        let now = Instant::now();
        let first = now + Duration::from_secs(1);
        let second = now + Duration::from_secs(60);

        queue.arm_timeout(first);
        assert_eq!(queue.len(), 1);
        assert!(queue.pop_next(now).is_none(), "not due yet");

        // Re-pointing the *same* pending timer is the case `enqueue_timeout` cannot express:
        // it is a no-op while a timer is pending, whereas `arm_timeout` replaces the deadline.
        assert!(
            !queue.enqueue_timeout(second),
            "enqueue_timeout dedupes by kind"
        );
        queue.arm_timeout(second);
        assert_eq!(queue.len(), 1, "still exactly one timer");

        // The replacement deadline is honoured: the superseded `first` deadline passing must
        // not fire the timer.
        assert!(queue.pop_next(first + Duration::from_millis(1)).is_none());
        assert_eq!(queue.len(), 1);

        let task = queue
            .pop_next(second)
            .expect("due at the replacement deadline");
        assert_eq!(task.kind(), PortTaskKind::Timeout);
        assert_eq!(task.deadline(), Some(second));
        assert!(queue.is_empty());
    }

    #[test]
    fn remove_and_clear_drop_tasks() {
        let mut queue = PortTaskQueue::new();
        assert!(queue.enqueue(PortTaskKind::Input));
        assert!(queue.remove(PortTaskKind::Input));
        assert!(!queue.remove(PortTaskKind::Input), "no longer pending");
        assert!(queue.is_empty());

        assert!(queue.enqueue(PortTaskKind::Output));
        assert!(queue.enqueue(PortTaskKind::Timeout));
        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn readiness_maps_eof_and_errors_onto_read_readiness() {
        // Plain readability / writability.
        let r = Readiness {
            readable: true,
            ..Readiness::default()
        };
        assert!(r.read_ready() && !r.write_ready() && !r.is_idle());

        let w = Readiness {
            writable: true,
            ..Readiness::default()
        };
        assert!(!w.read_ready() && w.write_ready());

        // A pure EOF/HUP event (the case the old inline dispatch dropped).
        let eof = Readiness {
            read_closed: true,
            ..Readiness::default()
        };
        assert!(!eof.readable);
        assert!(eof.read_ready(), "EOF must still reach ready_input");
        assert!(!eof.write_ready());

        // An error condition is readiness for **both** directions: the C reactor re-triggers
        // every armed event on `ERTS_POLL_EV_ERR` (`EPOLLERR | EPOLLHUP`).
        let err = Readiness {
            error: true,
            ..Readiness::default()
        };
        assert!(err.read_ready());
        assert!(err.write_ready());
        assert!(!err.is_idle());

        // A half-closed write side schedules the output task.
        let wclosed = Readiness {
            write_closed: true,
            ..Readiness::default()
        };
        assert!(wclosed.write_ready());
        assert!(!wclosed.read_ready());

        // Nothing set schedules nothing.
        assert!(Readiness::default().is_idle());
        assert!(!Readiness::default().read_ready());
        assert!(!Readiness::default().write_ready());
    }

    #[test]
    fn a_poll_error_schedules_both_directions() {
        // The legacy reactor turns a poll error into *every* armed event:
        // `ERTS_POLL_EV_ERR == EPOLLERR | EPOLLHUP` (`erl_poll.h:184`) and
        // `revents = state->active_events` (`erl_check_io.c:2018-2027`), after which both the
        // out and the in callback fire. A pure `EPOLLERR` (no `EPOLLHUP`) must therefore be
        // ready in *both* directions here.
        let err = Readiness {
            error: true,
            ..Readiness::default()
        };
        assert!(err.read_ready(), "a poll error reaches ready_input");
        assert!(
            err.write_ready(),
            "a poll error reaches ready_output as well"
        );
        assert!(!err.is_idle());

        // The pre-existing rule is unchanged: a pure close (no error bit) is still not write
        // readiness, so a read-only port's full close schedules no `OUTPUT` task.
        let eof = Readiness {
            read_closed: true,
            ..Readiness::default()
        };
        assert!(eof.read_ready() && !eof.write_ready());
    }

    #[test]
    fn a_pure_poll_error_schedules_output_only_for_an_armed_write_direction() {
        // The C's rule end to end: `ERTS_POLL_EV_ERR` (`EPOLLERR | EPOLLHUP`, `erl_poll.h:184`)
        // re-triggers *every* selected event (`revents = state->active_events`,
        // `erl_check_io.c:2018-2027`), so a fully armed fd fires both `oready` and `iready`
        // (`:2099-2110`) — while only the directions actually selected are fired.
        let mut poller = Poller::new().expect("a poll set");
        let mut registry = PortRegistry::new();

        // A port that armed BOTH directions: the error bit reaches `write_ready` too, so an
        // `Output` task is scheduled alongside the `Input` task.
        let (stream, _peer) = UnixStream::pair().expect("a socketpair");
        let id = registry
            .open(
                &mut poller,
                Box::new(BothDirectionsDriver {
                    fd: stream.as_raw_fd(),
                    token: None,
                }),
            )
            .expect("the both-directions port opens");
        let err = Readiness {
            error: true,
            ..Readiness::default()
        };
        assert!(registry.schedule_readiness(id.number(), err));
        assert!(
            registry
                .get(id)
                .expect("live")
                .has_pending_task(PortTaskKind::Input),
            "a poll error reaches the armed read direction"
        );
        assert!(
            registry
                .get(id)
                .expect("live")
                .has_pending_task(PortTaskKind::Output),
            "a poll error reaches the armed write direction too"
        );

        // A read-only port (`FdDriver::pipe` arms only `READABLE`): the armed-direction gate
        // keeps the error bit from scheduling a direction the driver never armed.
        let read_only = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("a second, read-armed port");
        assert!(registry.schedule_readiness(read_only.number(), err));
        assert!(registry
            .get(read_only)
            .expect("live")
            .has_pending_task(PortTaskKind::Input));
        assert!(
            !registry
                .get(read_only)
                .expect("live")
                .has_pending_task(PortTaskKind::Output),
            "an unarmed write direction is never scheduled, whatever the readiness"
        );

        registry.close(&mut poller, id).expect("closes");
        registry.close(&mut poller, read_only).expect("closes");
        assert!(
            poller.is_empty(),
            "closing deregisters both ports' descriptors"
        );
    }

    #[test]
    fn readiness_converts_from_a_poller_ready_mask() {
        // `Ready` has no public constructor outside `poller`, so only its `Default` value is
        // reachable here; the conversion is a field-by-field copy, and the meaningful cases
        // are pinned through `Readiness` itself above.
        let from_default: Readiness = Ready::default().into();
        assert_eq!(from_default, Readiness::default());
        assert!(from_default.is_idle());
    }
}