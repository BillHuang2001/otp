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

//! Sequential tracing (contract A.8): the `erlang:seq_trace/…`/`seq_trace_info/1`/
//! `seq_trace_print/…` BIF machinery — the per-process trace token (label/serial/flags/tracer)
//! and the `{seq_trace, …}` event output.
//!
//! Sequential tracing follows **one** information flow — the messages a trace token travels
//! with — from the process that started it. A runtime's half of it is small, entirely local
//! state:
//!
//! * **the trace token** — the C's mutable 5-tuple `{Flags, Label, Serial, From, Lastcnt}`
//!   ([`Token`], `erl_process.h:1437-1448`), one per process, absent (the Erlang `[]`) while
//!   sequential tracing is inactive for that process;
//! * **the counters** — the node-global serial [`SeqTrace::clock`] a token's `Serial` is
//!   stamped from, plus each process's *last received* counter (`lastcnt`), which survives
//!   the token being cleared exactly as the C's `p->seq_trace_lastcnt` does;
//! * **the flag bitmask** — [`FLAGS`]/[`flag_bit`]/[`decode_flags`]/[`encode_flags`] mirror
//!   `seq_trace.erl`'s `?SEQ_TRACE_*` constants and its `decode_flags/1`;
//! * **the system logger** — [`SystemLogger`], the node-global `system_logger` flag the C
//!   keeps beside the sequential-tracer state: exchanged by
//!   `erlang:system_flag(system_logger, _)` (`bif.c:5345-5348`) and reported by
//!   `erlang:system_info(system_logger)` (`erl_bif_info.c:3560-3561`);
//! * **the output** — [`RuntimeContext::seq_trace_message`] builds the documented
//!   `{seq_trace, Label, {print, Serial, From, _, Info}}` message a system tracer receives,
//!   and [`RuntimeContext::seq_trace_send_message`] its `{send, …}` sibling (the event the
//!   send path emits when the sender's token has [`FLAG_SEND`]).
//!
//! The whole model is **pure**: [`SeqTrace`] holds no runtime borrow, allocates nothing and
//! can be unit-tested on its own; only the term-building half
//! ([`RuntimeContext::seq_trace_2`] and friends, plus [`RuntimeContext::sequential_trace_token`])
//! lives in the BIF layer, because a term needs the engine's heap seam.
//!
//! # What is implemented
//!
//! All four BIFs `erts/emulator/beam/bif.tab:220-223` declares — `erlang:seq_trace/2`,
//! `erlang:seq_trace_info/1`, `erlang:seq_trace_print/1` and `erlang:seq_trace_print/2` — plus
//! the `reset_seq_trace` and `system_logger` arms of `erlang:system_flag/2`, which are the
//! calls the kernel module's `seq_trace:reset_trace/0` and `logger_proxy:init/1`
//! (`lib/kernel/src/logger_proxy.erl:117`) make. With the kernel module (whose `set_token/1,2`,
//! `get_token/0,1`, `print/1,2` and `reset_trace/0` are plain Erlang this runtime executes)
//! that covers the whole documented token API, and `process_info(Pid,
//! sequential_trace_token)` answers the item `seq_trace:get_token/0` is written on.
//!
//! # Deliberately out of scope (a later stage)
//!
//! * **Token propagation.** A token riding on a sent message and the `receive` side stamping
//!   `From`/`Serial`/`Lastcnt` from the envelope (and the `{'receive', …}` event that follows).
//!   The **send** event *is* emitted, by the send path, which stamps the sending process's own
//!   token; nothing here touches `beam_message`/`beam_process`, so the token is not carried
//!   with the message.
//! * **The system tracer.** `erlang:system_flag(sequential_tracer, _)` (and therefore
//!   `seq_trace:set_system_tracer/1`) is not implemented, so [`SeqTrace::tracer`] stays `None`
//!   and the message a `print` builds has no recipient. That is **not** a divergence: the
//!   oracle builds the same term and hands it to a `nil` tracer, which drops it
//!   (`erl_trace.c:981-1024`), and it stamps the token either way — which is why
//!   `seq_trace:print/1` advances the serial even when nothing is delivered.
//! * The ordinary tracing subsystem (`erlang:trace/3`, match specs, `{trace, …}` messages) is
//!   a different surface, owned by [`crate::trace`].
//!
//! Legacy C ground truth (read-only): `../../erts/emulator/beam/erl_bif_trace.c`
//! (`erts_seq_trace`, `erl_seq_trace_info`, `seq_trace_print_1,2`), `erl_trace.{c,h}`
//! (`seq_trace_update_serial`, `seq_trace_output_generic`) and `bif.c` (`system_flag_2`'s
//! `reset_seq_trace` arm); the Erlang side is `../../lib/kernel/src/seq_trace.erl`.

use std::collections::HashMap;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_process::ProcessId;
use beam_term::{compare, is_small, make_small, signed_value, Eterm, NIL};

use crate::context::RuntimeContext;

// ---------------------------------------------------------------------------------------
// The flag bitmask (`seq_trace.erl:373-378`)
// ---------------------------------------------------------------------------------------

/// `?SEQ_TRACE_SEND` — trace information **sends**.
pub const FLAG_SEND: u32 = 1;
/// `?SEQ_TRACE_RECEIVE` — trace information **receives**.
pub const FLAG_RECEIVE: u32 = 2;
/// `?SEQ_TRACE_PRINT` — trace explicit `seq_trace:print/1,2` calls.
pub const FLAG_PRINT: u32 = 4;
/// `?SEQ_TRACE_NOW_TIMESTAMP` — stamp each traced event with a timestamp.
pub const FLAG_TIMESTAMP: u32 = 8;
/// `?SEQ_TRACE_STRICT_MON_TIMESTAMP` — strict monotonic timestamps.
pub const FLAG_STRICT_MONOTONIC_TIMESTAMP: u32 = 16;
/// `?SEQ_TRACE_MON_TIMESTAMP` — `erlang:monotonic_time/0` timestamps.
pub const FLAG_MONOTONIC_TIMESTAMP: u32 = 32;

/// The six flag components and their bits, in the order `seq_trace.erl`'s `decode_flags/1`
/// lists them (`lib/kernel/src/seq_trace.erl:606-615`).
pub const FLAGS: &[(&str, u32)] = &[
    ("print", FLAG_PRINT),
    ("send", FLAG_SEND),
    ("receive", FLAG_RECEIVE),
    ("timestamp", FLAG_TIMESTAMP),
    ("strict_monotonic_timestamp", FLAG_STRICT_MONOTONIC_TIMESTAMP),
    ("monotonic_timestamp", FLAG_MONOTONIC_TIMESTAMP),
];

/// The bit the flag component `name` stands for, or `None` when `name` is not a flag.
///
/// The C's `erts_seq_trace` uses the same split: the six flag names are the ones that take a
/// boolean value, every other atom is a different component (or `badarg`).
pub fn flag_bit(name: &str) -> Option<u32> {
    FLAGS
        .iter()
        .find(|(flag, _)| *flag == name)
        .map(|(_, bit)| *bit)
}

/// Decodes a flags word into `(component, enabled)` pairs, in [`FLAGS`] order — the same
/// answer `seq_trace.erl`'s `decode_flags/1` computes, without the `band` calls.
pub fn decode_flags(flags: u32) -> Vec<(&'static str, bool)> {
    FLAGS
        .iter()
        .map(|(name, bit)| (*name, flags & bit != 0))
        .collect()
}

/// Encodes `(component, enabled)` pairs into a flags word — the inverse of [`decode_flags`].
///
/// `None` when a component name is not one of the six flags, so an unknown component is a
/// type error at the boundary rather than a silently ignored bit.
pub fn encode_flags(components: &[(&str, bool)]) -> Option<u32> {
    let mut flags = 0;
    for (name, enabled) in components {
        let bit = flag_bit(name)?;
        if *enabled {
            flags |= bit;
        }
    }
    Some(flags)
}

// ---------------------------------------------------------------------------------------
// The token and its state
// ---------------------------------------------------------------------------------------

/// One process's sequential-trace token — the C's 5-tuple
/// `{Flags, Label, Serial, From, Lastcnt}` (`erl_process.h:1437-1448`).
///
/// `flags` is the [`FLAGS`] bitmask, `label`/`from` are the raw terms the token carries (any
/// label term, the sender's pid term) and `serial`/`lastcnt` are the two counters
/// `seq_trace:get_token/0` reports as the `Serial` pair `{Lastcnt, Serial}`.
///
/// The two `Eterm` fields point into a heap, which is safe here for the same reason the
/// persistent-term store is safe: Milestone-1 runs no garbage collector, so every term this
/// runtime builds is pointer-stable for the runtime's lifetime (the Stage-4 collector must
/// treat this state as a root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// The enabled flags, as a [`FLAGS`] bitmask.
    pub flags: u32,
    /// The trace label — the term identifying the trace; the integer `0` until it is set.
    pub label: Eterm,
    /// The counter this process stamps its own events with.
    pub serial: u32,
    /// The pid term of the process that last stamped the token.
    pub from: Eterm,
    /// The counter received along with the token (the C's `seq_trace_lastcnt`).
    pub lastcnt: u32,
}

impl Token {
    /// The C's `new_seq_trace_token` defaults (`erl_bif_trace.c:3133-3144`): no flags, the
    /// label `0`, the serial `0`, and the given `from` pid term / `lastcnt` counter.
    pub fn new(from: Eterm, lastcnt: u32) -> Token {
        Token {
            flags: 0,
            label: make_small(0),
            serial: 0,
            from,
            lastcnt,
        }
    }
}

/// A process's sequential-trace record: its token (absent while tracing is inactive for it)
/// and the C's separate `p->seq_trace_lastcnt` PCB field.
///
/// The two are stored apart because the counter **outlives** the token: the C's
/// `set_token([])` clears `p->seq_trace_token` but leaves `p->seq_trace_lastcnt` alone, so a
/// token created afterwards still carries that counter (`erl_bif_trace.c:3109-3125`,
/// `new_seq_trace_token`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProcessTrace {
    /// The process's token, or `None` when sequential tracing is inactive for it.
    token: Option<Token>,
    /// The serial counter last received with a token.
    lastcnt: u32,
}

/// The node-global **system logger** — the C's `system_logger` (`erl_trace.c:2479-2491`):
/// the process the emulator hands logger messages to (`erts_queue_error_logger_message`,
/// `erl_trace.c:2493-2497`) and the value `erlang:system_info(system_logger)` reports
/// (`erl_bif_info.c:3560-3561`).
///
/// `erl_trace.c:339` initialises the C's atomic to the atom `logger`, and
/// `erts_set_system_logger` (`erl_trace.c:2485-2491`) accepts exactly three values: the atom
/// `logger`, the atom `undefined` and an internal pid — anything else is the C's
/// `THE_NON_VALUE`, which `bif.c:5345-5348` turns into `badarg`. The pid is accepted on
/// `is_internal_pid` alone (no liveness predicate), so a dead pid is stored just as the C
/// stores it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SystemLogger {
    /// The C's initial value, `am_logger` (`erl_trace.c:339`): the atom `logger`.
    #[default]
    Logger,
    /// The atom `undefined` — no system logger, what `logger_proxy` sets as it shuts down
    /// (`lib/kernel/src/logger_proxy.erl:147`).
    Undefined,
    /// An internal pid, as `logger_proxy`'s start-up sets it
    /// (`erlang:system_flag(system_logger, self())`, `lib/kernel/src/logger_proxy.erl:117`).
    Pid(ProcessId),
}

/// This runtime's sequential-trace state (the whole of it, and the type the
/// [`Runtime`](crate::Runtime) owns).
///
/// The state is *pure*: no runtime borrow, no allocation, no term building. `ProcessId` keys
/// are the pids the tokens belong to, so a token is per-process exactly as in the C, while
/// the serial clock and the system tracer are node-global (the C has no node-global tracer
/// either: `erts_get_system_seq_tracer` is one value for the emulator).
#[derive(Debug, Clone, Default)]
pub struct SeqTrace {
    /// The per-process records, created lazily by [`SeqTrace::ensure`].
    processes: HashMap<ProcessId, ProcessTrace>,
    /// The serial clock a token's `Serial` is stamped from (the C's per-process
    /// `seq_trace_clock`, modelled node-globally here because Milestone-1 has no token
    /// propagation between processes; [`SeqTrace::set_serial`] reseeds it exactly as the C
    /// does, and [`SeqTrace::reset`] zeroes it).
    clock: u64,
    /// The **system tracer** a `print` event is handed to, or `None` when none is configured.
    ///
    /// Nothing can configure one yet — `erlang:system_flag(sequential_tracer, _)` is not
    /// implemented — so a built event message is dropped, which is what the oracle does with
    /// no tracer configured (see the module docs). The field is the seam that flag will use.
    tracer: Option<ProcessId>,
    /// The node-global **system logger** ([`SystemLogger`]) — the C's `system_logger`
    /// atomic. Initialised to the C's `am_logger` (`erl_trace.c:339`) and deliberately left
    /// out of [`SeqTrace::reset`], which is the C's `reset_seq_trace` (it clears the tokens
    /// and the clock, not the logger).
    system_logger: SystemLogger,
}

impl SeqTrace {
    /// A state with no tokens, a zeroed clock and no system tracer.
    pub fn new() -> SeqTrace {
        SeqTrace::default()
    }

    /// `pid`'s token, or `None` while sequential tracing is inactive for it.
    pub fn token(&self, pid: ProcessId) -> Option<Token> {
        self.processes.get(&pid).and_then(|state| state.token)
    }

    /// Whether `pid` currently has a token — the C's `have_seqtrace`
    /// (`erl_message.h:222-226`).
    pub fn is_active(&self, pid: ProcessId) -> bool {
        self.token(pid).is_some()
    }

    /// `pid`'s token, created with the C's defaults when it has none — the C's
    /// `new_seq_trace_token` (`erl_if_trace.c:3133-3154`).
    pub fn ensure(&mut self, pid: ProcessId, from: Eterm) -> Token {
        *self.token_mut(pid, from)
    }

    /// The mint-or-fetch half of [`SeqTrace::ensure`], so every mutator shares one creation
    /// rule (`Option::get_or_insert` — no panicking branch on the internal invariant).
    fn token_mut(&mut self, pid: ProcessId, from: Eterm) -> &mut Token {
        let state = self.processes.entry(pid).or_default();
        let lastcnt = state.lastcnt;
        state
            .token
            .get_or_insert_with(|| Token::new(from, lastcnt))
    }

    /// Clears `pid`'s token — `erlang:seq_trace(sequential_trace_token, [])`, which is what
    /// `seq_trace:set_token([])` calls — returning the token that was there.
    ///
    /// The process's `lastcnt` counter is deliberately **kept** (see [`ProcessTrace`]).
    pub fn clear(&mut self, pid: ProcessId) -> Option<Token> {
        self.processes
            .get_mut(&pid)
            .and_then(|state| state.token.take())
    }

    /// Sets or clears one flag bit, returning the flag's **previous** value — the
    /// `erlang:seq_trace(Flag, Bool)` answer.
    pub fn set_flag(&mut self, pid: ProcessId, from: Eterm, bit: u32, enabled: bool) -> bool {
        let token = self.token_mut(pid, from);
        let previous = token.flags & bit != 0;
        if enabled {
            token.flags |= bit;
        } else {
            token.flags &= !bit;
        }
        previous
    }

    /// Sets the label, returning the **previous** label — the `erlang:seq_trace(label, L)`
    /// answer (`0` for a token that has just been created).
    pub fn set_label(&mut self, pid: ProcessId, from: Eterm, label: Eterm) -> Eterm {
        let token = self.token_mut(pid, from);
        let previous = token.label;
        token.label = label;
        previous
    }

    /// Sets the `{Previous, Current}` serial pair, returning the **old** pair —
    /// `erlang:seq_trace(serial, {Lastcnt, Serial})`.
    ///
    /// As in the C (`erl_bif_trace.c:3103-3106`) the pair reseeds the serial clock and the
    /// process's own `lastcnt` counter along with the token's fields.
    pub fn set_serial(
        &mut self,
        pid: ProcessId,
        from: Eterm,
        lastcnt: u32,
        serial: u32,
    ) -> (u32, u32) {
        let token = self.token_mut(pid, from);
        let previous = (token.lastcnt, token.serial);
        token.lastcnt = lastcnt;
        token.serial = serial;
        // The pair reseeds the **process's** counter as well as the token's (the C writes
        // `p->seq_trace_lastcnt`, `erl_bif_trace.c:3103-3106`), which is what a later
        // `advance_serial` copies back into a freshly stamped token.
        self.processes.entry(pid).or_default().lastcnt = lastcnt;
        self.clock = u64::from(serial);
        previous
    }

    /// The C's `seq_trace_update_serial` (`erl_trace.c:919-940`): stamp the token with this
    /// process as `From`, bump the serial clock and copy the process's `lastcnt` into the
    /// token. `None` — and no counter touched — when there is no token, exactly as the C
    /// returns 0 without stamping.
    ///
    /// The bumped token is returned, because the event message a `print` builds carries the
    /// **stamped** values.
    pub fn advance_serial(&mut self, pid: ProcessId, from: Eterm) -> Option<Token> {
        let (active, lastcnt) = match self.processes.get(&pid) {
            Some(state) => (state.token.is_some(), state.lastcnt),
            None => return None,
        };
        if !active {
            return None;
        }
        self.clock += 1;
        // The C's clock is a `Uint32`, so an overflowing serial wraps; a `u64` clock
        // truncated to `u32` reproduces that exactly.
        let serial = (self.clock & 0xFFFF_FFFF) as u32;
        let token = self.token_mut(pid, from);
        token.from = from;
        token.serial = serial;
        token.lastcnt = lastcnt;
        Some(*token)
    }

    /// The serial clock's current value. Exposed for tests and for a caller that wants to
    /// report it; the token's `Serial` field is what the observable surface shows.
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// The **system tracer** a `print` event is handed to (`None` when none is configured).
    pub fn tracer(&self) -> Option<ProcessId> {
        self.tracer
    }

    /// Sets the system tracer, returning the previous one.
    ///
    /// The seam `erlang:system_flag(sequential_tracer, Tracer)` will drive; that flag is not
    /// implemented, so production code leaves the tracer unset — and then a `print` event is
    /// built and dropped, exactly like the oracle's `nil` tracer.
    pub fn set_tracer(&mut self, tracer: Option<ProcessId>) -> Option<ProcessId> {
        std::mem::replace(&mut self.tracer, tracer)
    }

    /// The node-global **system logger** ([`SystemLogger`]) — the C's `system_logger`
    /// atomic, and the value `erlang:system_info(system_logger)` reports.
    pub fn system_logger(&self) -> SystemLogger {
        self.system_logger
    }

    /// Sets the system logger, returning the **previous** value — the
    /// `erlang:system_flag(system_logger, Logger)` answer, and the C's
    /// `erts_set_system_logger` exchange (`erl_trace.c:2485-2491`).
    pub fn set_system_logger(&mut self, logger: SystemLogger) -> SystemLogger {
        std::mem::replace(&mut self.system_logger, logger)
    }
    /// `erlang:system_flag(reset_seq_trace, _)` (`bif.c:5198-5223`): every process's token is
    /// cleared and both counters are zeroed, which is what stops all ongoing sequential
    /// tracing on the node. The system tracer is deliberately left alone (the C does not
    /// touch it either).
    pub fn reset(&mut self) {
        self.processes.clear();
        self.clock = 0;
    }
}

// ---------------------------------------------------------------------------------------
// The BIF layer
// ---------------------------------------------------------------------------------------

impl RuntimeContext<'_> {
    /// `erlang:seq_trace(Type, Value)` — the C's `erts_seq_trace`
    /// (`erl_bif_trace.c:3035-3130`).
    ///
    /// | `Type` | `Value` | Answer |
    /// |---|---|---|
    /// | `send`/`receive`/`print`/`timestamp`/`strict_monotonic_timestamp`/`monotonic_timestamp` | `true`/`false` | the flag's **previous** value, as `true`/`false` |
    /// | `label` | any term | the **previous** label |
    /// | `serial` | `{Previous, Current}` (two small integers) | the **previous** pair `{Lastcnt, Serial}` |
    /// | `sequential_trace_token` | `[]` | the **previous** token, or `[]` |
    ///
    /// Every other `(Type, Value)` pair is `badarg`; a `Type` that is not an atom is `badarg`.
    pub(crate) fn seq_trace_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let component = *args.first().ok_or(BeamError::BadArg)?;
        let value = *args.get(1).ok_or(BeamError::BadArg)?;
        // A non-atom `Type` cannot name a component: the C rejects it up front.
        let name = self.atom_name(component).ok_or(BeamError::BadArg)?;
        let pid = self.pid();
        let from = pid.to_eterm();

        // The six flag components. A flag name with a non-boolean value falls through to the
        // component checks below in the C and ends in `badarg`; there is nothing for it to
        // match there either, so the answer is the same.
        if let Some(bit) = flag_bit(&name) {
            let enabled = match self.atom_name(value).as_deref() {
                Some("true") => true,
                Some("false") => false,
                _ => return Err(BeamError::BadArg),
            };
            let previous = self.runtime.seq_trace_mut().set_flag(pid, from, bit, enabled);
            return self.atom(if previous { "true" } else { "false" });
        }

        match name.as_str() {
            "label" => Ok(self.runtime.seq_trace_mut().set_label(pid, from, value)),
            "serial" => {
                let (lastcnt, serial) = serial_pair(value)?;
                let previous = self
                    .runtime
                    .seq_trace_mut()
                    .set_serial(pid, from, lastcnt, serial);
                let previous_lastcnt = make_small(i64::from(previous.0));
                let previous_serial = make_small(i64::from(previous.1));
                terms::make_tuple(self, &[previous_lastcnt, previous_serial])
            }
            "sequential_trace_token" => {
                // The C accepts only `[]` here: a non-empty argument is `badarg`, never a
                // silently ignored token (`erl_bif_trace.c:3110-3112`).
                if value != NIL {
                    return Err(BeamError::BadArg);
                }
                match self.runtime.seq_trace_mut().clear(pid) {
                    Some(token) => self.sequential_trace_token(token),
                    None => Ok(NIL),
                }
            }
            _ => Err(BeamError::BadArg),
        }
    }

    /// `erlang:seq_trace_info(Item)` — the C's `erl_seq_trace_info`
    /// (`erl_bif_trace.c:3157-3214`).
    ///
    /// The answer is always a `{Item, Value}` pair: a flag answers `true`/`false` (with **no**
    /// token every flag — and the legacy `spawn` item — answers `false`), `label` answers the
    /// label, `serial` answers `{Lastcnt, Serial}`. Without a token, `label`/`serial` answer
    /// the bare `[]`; any other item — with or without a token — is `badarg` (so an unknown
    /// item is a genuine argument error, not a capability gap).
    pub(crate) fn seq_trace_info_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let item = *args.first().ok_or(BeamError::BadArg)?;
        let name = self.atom_name(item).ok_or(BeamError::BadArg)?;
        let token = self.runtime.seq_trace().token(self.pid());
        let value = match token {
            None => {
                if name == "spawn" || flag_bit(&name).is_some() {
                    self.atom("false")?
                } else if name == "label" || name == "serial" {
                    return Ok(NIL);
                } else {
                    return Err(BeamError::BadArg);
                }
            }
            Some(token) => {
                if let Some(bit) = flag_bit(&name) {
                    self.atom(if token.flags & bit != 0 { "true" } else { "false" })?
                } else if name == "label" {
                    token.label
                } else if name == "serial" {
                    let lastcnt = make_small(i64::from(token.lastcnt));
                    let serial = make_small(i64::from(token.serial));
                    terms::make_tuple(self, &[lastcnt, serial])?
                } else {
                    return Err(BeamError::BadArg);
                }
            }
        };
        terms::make_tuple(self, &[item, value])
    }

    /// `erlang:seq_trace_print(Info)` — the C's `seq_trace_print_1`
    /// (`erl_bif_trace.c:3229-3238`).
    ///
    /// `false` while sequential tracing is inactive for this process; otherwise the token is
    /// stamped ([`SeqTrace::advance_serial`]) and the `print` event is emitted, and the answer
    /// is `true` — "true is returned if the conditions to pass the message are fulfilled",
    /// with nothing actually delivered while no system tracer is configured.
    pub(crate) fn seq_trace_print_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let info = *args.first().ok_or(BeamError::BadArg)?;
        let pid = self.pid();
        let Some(token) = self
            .runtime
            .seq_trace_mut()
            .advance_serial(pid, pid.to_eterm())
        else {
            return self.atom("false");
        };
        self.seq_trace_emit(token, info)?;
        self.atom("true")
    }

    /// `erlang:seq_trace_print(Label, Info)` — the C's `seq_trace_print_2`
    /// (`erl_bif_trace.c:3248-3259`).
    ///
    /// As `seq_trace_print/1`, but the event is emitted only when `Label` is **equal** to the
    /// token's label (the C's `EQ`, i.e. structural Erlang equality — a boxed label compares
    /// by value, not by address). A different label answers `false` and does **not** stamp the
    /// token.
    pub(crate) fn seq_trace_print_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let label = *args.first().ok_or(BeamError::BadArg)?;
        let info = *args.get(1).ok_or(BeamError::BadArg)?;
        let pid = self.pid();
        let Some(token) = self.runtime.seq_trace().token(pid) else {
            return self.atom("false");
        };
        let matches = compare::eq(label, token.label, self.runtime.code_server.atoms());
        if !matches {
            return self.atom("false");
        }
        let Some(token) = self
            .runtime
            .seq_trace_mut()
            .advance_serial(pid, pid.to_eterm())
        else {
            return self.atom("false");
        };
        self.seq_trace_emit(token, info)?;
        self.atom("true")
    }

    /// `erlang:system_flag(Flag, Value)` — the `reset_seq_trace` arm of the C's
    /// `system_flag_2` (`bif.c:5198-5223`), which is the call `seq_trace:reset_trace/0`
    /// makes, and the `system_logger` arm (`bif.c:5345-5348` →
    /// `erts_set_system_logger`, `erl_trace.c:2485-2491`), which is the call
    /// `logger_proxy:init/1` makes (`lib/kernel/src/logger_proxy.erl:117`).
    ///
    /// `reset_seq_trace` clears every process's token and zeroes both counters and answers
    /// `true` (the C ignores the second argument, so any value is accepted).
    ///
    /// `system_logger` accepts exactly the C's three values — the atom `logger`, the atom
    /// `undefined`, and an internal pid — and answers the **previous** value (the atom
    /// `logger`/`undefined`, or the pid). Anything else is the C's `THE_NON_VALUE`,
    /// i.e. `badarg` (so `42` and `{a}` are `badarg`, exactly as the oracle answers).
    /// A `Flag` that is not an atom is `badarg`; **every** other atom is the honest typed
    /// capability gap — `system_flag/2`'s other flags (scheduling, `sequential_tracer`,
    /// distribution, …) are runtime capabilities this stage does not have, and claiming
    /// `badarg` for one would be a lie about a real OTP flag.
    pub(crate) fn system_flag_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let flag = *args.first().ok_or(BeamError::BadArg)?;
        let Some(name) = self.atom_name(flag) else {
            return Err(BeamError::BadArg);
        };
        if name == "reset_seq_trace" {
            self.runtime.seq_trace_mut().reset();
            return self.atom("true");
        }
        if name == "system_logger" {
            let value = *args.get(1).ok_or(BeamError::BadArg)?;
            let logger = self.decode_system_logger(value)?;
            let previous = self.runtime.seq_trace_mut().set_system_logger(logger);
            return self.encode_system_logger(previous);
        }
        Err(crate::error::unsupported(&format!(
            "erlang:system_flag({name}, _) (only `reset_seq_trace` and `system_logger` are implemented)"
        )))
    }

    /// Decodes `erlang:system_flag(system_logger, Value)`'s argument the way the C's
    /// `erts_set_system_logger` (`erl_trace.c:2485-2491`) does: the atom `logger`, the atom
    /// `undefined`, or an internal pid. Every other term is the C's `THE_NON_VALUE`, i.e.
    /// [`BeamError::BadArg`]. Liveness is deliberately **not** checked — the C tests
    /// `is_internal_pid` alone, so a pid that is no longer alive is still accepted.
    fn decode_system_logger(&mut self, value: Eterm) -> Result<SystemLogger> {
        match self.atom_name(value).as_deref() {
            Some("logger") => Ok(SystemLogger::Logger),
            Some("undefined") => Ok(SystemLogger::Undefined),
            _ => ProcessId::from_eterm(value)
                .map(SystemLogger::Pid)
                .map_err(|_| BeamError::BadArg),
        }
    }

    /// The term `erlang:system_flag(system_logger, _)` answers for `logger` — the previous
    /// value — and the term `erlang:system_info(system_logger)` reports: the atom
    /// `logger`/`undefined`, or the pid.
    fn encode_system_logger(&mut self, logger: SystemLogger) -> Result<Eterm> {
        match logger {
            SystemLogger::Logger => self.atom("logger"),
            SystemLogger::Undefined => self.atom("undefined"),
            SystemLogger::Pid(pid) => Ok(pid.to_eterm()),
        }
    }
    /// The `{Flags, Label, Serial, From, Lastcnt}` tuple of `token` — the shape
    /// `erlang:seq_trace/2` answers for the `sequential_trace_token` component, and the value
    /// `process_info(Pid, sequential_trace_token)` reports (`erl_bif_info.c:2162-2166` copies
    /// the token tuple verbatim).
    pub(crate) fn sequential_trace_token(&mut self, token: Token) -> Result<Eterm> {
        let flags = make_small(i64::from(token.flags));
        let serial = make_small(i64::from(token.serial));
        let lastcnt = make_small(i64::from(token.lastcnt));
        terms::make_tuple(self, &[flags, token.label, serial, token.from, lastcnt])
    }

    /// The documented sequential-trace output message
    /// (`lib/kernel/src/seq_trace.erl:37-90`, the C's `seq_trace_output_generic`,
    /// `erl_trace.c:968-1029`):
    ///
    /// ```text
    /// {seq_trace, Label, {print, {Lastcnt, Serial}, From, _, Info}}
    /// ```
    ///
    /// The fourth payload element is the C's `receiver`, which is `[]` for a `print` event
    /// (`seq_trace_output_generic` is called with `NIL`). The same builder serves the
    /// `{send, …}`/`{'receive', …}` events — [`Self::seq_trace_send_message`] is its `send`
    /// form — which is why the shape is built here rather than inline in the caller.
    pub(crate) fn seq_trace_message(&mut self, token: Token, info: Eterm) -> Result<Eterm> {
        self.seq_trace_output("print", token, NIL, info)
    }

    /// The sequential-trace **send** event
    /// (`lib/kernel/src/seq_trace.erl:62-68`):
    ///
    /// ```text
    /// {seq_trace, Label, {send, {Lastcnt, Serial}, From, To, Message}}
    /// ```
    ///
    /// `From` is the stamped token's process, `To` the destination as the send carried it and
    /// `Message` the message itself; `Serial` is the same `{PreviousSerial, ThisSerial}` pair
    /// a `print` event reports. The send path emits it ([`crate::trace`]'s
    /// `RuntimeContext::trace_send_events`) when the sender's token has [`FLAG_SEND`]; a token
    /// with the `timestamp` flag does not add the documented fourth outer element yet.
    pub(crate) fn seq_trace_send_message(
        &mut self,
        token: Token,
        to: Eterm,
        message: Eterm,
    ) -> Result<Eterm> {
        self.seq_trace_output("send", token, to, message)
    }

    /// The shared builder behind [`Self::seq_trace_message`] and
    /// [`Self::seq_trace_send_message`]: the five-element payload `{Tag, {Lastcnt, Serial},
    /// From, Receiver, Info}` wrapped in `{seq_trace, Label, Payload}`.
    fn seq_trace_output(
        &mut self,
        tag: &str,
        token: Token,
        receiver: Eterm,
        info: Eterm,
    ) -> Result<Eterm> {
        let tag = self.atom(tag)?;
        let seq_trace = self.atom("seq_trace")?;
        let lastcnt = make_small(i64::from(token.lastcnt));
        let serial = make_small(i64::from(token.serial));
        let counters = terms::make_tuple(self, &[lastcnt, serial])?;
        let payload = terms::make_tuple(self, &[tag, counters, token.from, receiver, info])?;
        terms::make_tuple(self, &[seq_trace, token.label, payload])
    }

    /// Emits one `print` event: builds the message and hands it to the system tracer.
    ///
    /// Nothing can configure a system tracer yet ([`SeqTrace::tracer`] is `None`), so the
    /// message is built and dropped — the oracle's own behaviour with no tracer configured,
    /// where the same term is handed to a `nil` tracer (`erl_trace.c:981-1024`). `Ok(())` is
    /// the documented "the conditions to pass the message were fulfilled"; the BIF's `true`
    /// answer comes from the caller.
    fn seq_trace_emit(&mut self, token: Token, info: Eterm) -> Result<()> {
        let message = self.seq_trace_message(token, info)?;
        match self.runtime.seq_trace().tracer() {
            Some(tracer) => self.send_message(tracer.to_eterm(), message),
            None => Ok(()),
        }
    }
}

/// The `{Previous, Current}` serial argument of `erlang:seq_trace(serial, Pair)`, as the C
/// reads it (`erl_bif_trace.c:3088-3107`): a 2-tuple of two **small** integers.
///
/// Anything else — a non-tuple, a tuple of another arity, a float, an atom, a bignum — is
/// `badarg`, never a coerced value.
fn serial_pair(value: Eterm) -> Result<(u32, u32)> {
    if terms::tuple_arity(value)? != 2 {
        return Err(BeamError::BadArg);
    }
    let previous = terms::tuple_element(value, 0)?;
    let current = terms::tuple_element(value, 1)?;
    if !is_small(previous) || !is_small(current) {
        return Err(BeamError::BadArg);
    }
    // `unsigned_val` truncates to the C's `Uint32` counter, which is what a two's-complement
    // cast to `u32` does for the (60-bit) smalls this accepts.
    Ok((
        signed_value(previous) as u32,
        signed_value(current) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{make_small, NIL};

    /// A pid for the state tests — the map key, never a live process.
    fn pid(number: u64) -> ProcessId {
        ProcessId::new(number, 0)
    }

    /// The label `0` the C gives a freshly created token.
    fn zero() -> Eterm {
        make_small(0)
    }

    #[test]
    fn the_flag_bitmask_matches_seq_trace_erl() {
        assert_eq!(flag_bit("print"), Some(4));
        assert_eq!(flag_bit("send"), Some(1));
        assert_eq!(flag_bit("receive"), Some(2));
        assert_eq!(flag_bit("timestamp"), Some(8));
        assert_eq!(flag_bit("strict_monotonic_timestamp"), Some(16));
        assert_eq!(flag_bit("monotonic_timestamp"), Some(32));
        assert_eq!(flag_bit("label"), None);
        assert_eq!(flag_bit(""), None);
    }

    #[test]
    fn decode_and_encode_flags_are_inverses() {
        // `seq_trace.erl`'s `decode_flags/1` order: print, send, 'receive', then the three
        // timestamp flags.
        let decoded = decode_flags(FLAG_PRINT | FLAG_RECEIVE);
        assert_eq!(
            decoded,
            vec![
                ("print", true),
                ("send", false),
                ("receive", true),
                ("timestamp", false),
                ("strict_monotonic_timestamp", false),
                ("monotonic_timestamp", false),
            ]
        );
        assert_eq!(encode_flags(&decoded), Some(FLAG_PRINT | FLAG_RECEIVE));
        assert_eq!(encode_flags(&decode_flags(0)), Some(0));
        assert_eq!(encode_flags(&decode_flags(63)), Some(63));
        // An unknown component is a type error, not a silently dropped bit.
        assert_eq!(encode_flags(&[("label", true)]), None);
    }

    #[test]
    fn a_fresh_state_has_no_token() {
        let state = SeqTrace::new();
        assert_eq!(state.token(pid(1)), None);
        assert!(!state.is_active(pid(1)));
        assert_eq!(state.clock(), 0);
        assert_eq!(state.tracer(), None);
    }

    #[test]
    fn ensure_creates_the_c_defaults_and_is_idempotent() {
        let mut state = SeqTrace::new();
        let from = pid(1).to_eterm();
        let created = state.ensure(pid(1), from);
        assert_eq!(
            created,
            Token {
                flags: 0,
                label: zero(),
                serial: 0,
                from,
                lastcnt: 0,
            }
        );
        // A second `ensure` finds the same token.
        assert_eq!(state.ensure(pid(1), make_small(99)), created);
        // ... and calling with a different `from` does not rewrite it (the C's
        // `new_seq_trace_token` only allocates, it never stamps).
        assert_eq!(state.token(pid(1)), Some(created));
    }

    #[test]
    fn set_flag_reports_the_previous_value() {
        let mut state = SeqTrace::new();
        let from = pid(1).to_eterm();
        assert!(!state.set_flag(pid(1), from, FLAG_SEND, true));
        assert!(state.token(pid(1)).is_some_and(|t| t.flags == FLAG_SEND));
        assert!(state.set_flag(pid(1), from, FLAG_SEND, true));
        // Clearing reports the value that was there (`true`), then leaves the bit clear.
        assert!(state.set_flag(pid(1), from, FLAG_SEND, false));
        assert!(state.token(pid(1)).is_some_and(|t| t.flags == 0));
        // An independent process keeps its own token.
        assert!(!state.set_flag(pid(2), pid(2).to_eterm(), FLAG_PRINT, true));
        assert_eq!(state.token(pid(1)).map(|t| t.flags), Some(0));
        assert_eq!(state.token(pid(2)).map(|t| t.flags), Some(FLAG_PRINT));
    }

    #[test]
    fn set_label_reports_the_previous_label_and_accepts_any_term() {
        let mut state = SeqTrace::new();
        let from = pid(1).to_eterm();
        assert_eq!(state.set_label(pid(1), from, make_small(7)), zero());
        assert_eq!(state.token(pid(1)).map(|t| t.label), Some(make_small(7)));
        // A boxed label is stored verbatim (the C's `new_seq_trace_token` copies the term,
        // it never inspects it).
        assert_eq!(state.set_label(pid(1), from, NIL), make_small(7));
        assert_eq!(state.token(pid(1)).map(|t| t.label), Some(NIL));
    }

    #[test]
    fn set_serial_reports_the_previous_pair_and_reseeds_the_clock() {
        let mut state = SeqTrace::new();
        let from = pid(1).to_eterm();
        assert_eq!(state.set_serial(pid(1), from, 3, 5), (0, 0));
        assert_eq!(state.clock(), 5);
        assert_eq!(state.set_serial(pid(1), from, 8, 13), (3, 5));
        let token = state.token(pid(1)).expect("a token");
        assert_eq!((token.lastcnt, token.serial), (8, 13));
    }

    #[test]
    fn advance_serial_stamps_the_token_and_bumps_the_clock() {
        let mut state = SeqTrace::new();
        let from = pid(1).to_eterm();
        // No token: nothing is stamped and the clock does not move (the C returns 0).
        assert_eq!(state.advance_serial(pid(1), from), None);
        assert_eq!(state.clock(), 0);
        state.ensure(pid(1), make_small(4));
        state.set_serial(pid(1), from, 2, 0);
        let stamped = state.advance_serial(pid(1), from).expect("stamped");
        assert_eq!(stamped.serial, 1);
        // The process's own `lastcnt` is copied into the token, not the token's own field.
        assert_eq!(stamped.lastcnt, 2);
        assert_eq!(stamped.from, from);
        let second = state.advance_serial(pid(1), from).expect("stamped");
        assert_eq!(second.serial, 2);
        assert_eq!(state.clock(), 2);
        // A process without a token is still untouched.
        assert_eq!(state.advance_serial(pid(9), from), None);
        assert_eq!(state.clock(), 2);
    }

    #[test]
    fn the_lastcnt_counter_survives_clearing_the_token() {
        let mut state = SeqTrace::new();
        let from = pid(1).to_eterm();
        state.set_serial(pid(1), from, 4, 9);
        let cleared = state.clear(pid(1)).expect("the previous token");
        assert_eq!(cleared.lastcnt, 4);
        assert_eq!(state.token(pid(1)), None);
        assert!(!state.is_active(pid(1)));
        // A token created afterwards still carries the counter, exactly as the C's
        // `p->seq_trace_lastcnt` outlives `p->seq_trace_token = NIL`.
        assert_eq!(state.ensure(pid(1), from).lastcnt, 4);
        // Clearing an inactive process is the C's `[]` answer, not an error.
        assert_eq!(state.clear(pid(8)), None);
    }

    #[test]
    fn reset_clears_every_token_and_zeroes_the_counters() {
        let mut state = SeqTrace::new();
        state.set_serial(pid(1), pid(1).to_eterm(), 4, 9);
        state.set_flag(pid(2), pid(2).to_eterm(), FLAG_SEND, true);
        state.reset();
        assert_eq!(state.token(pid(1)), None);
        assert_eq!(state.token(pid(2)), None);
        assert_eq!(state.clock(), 0);
        // The counter a live process carries is zeroed too (the C sets
        // `p->seq_trace_lastcnt = 0`), so the next token starts from 0.
        assert_eq!(state.ensure(pid(1), pid(1).to_eterm()).lastcnt, 0);
        state.set_flag(pid(1), pid(1).to_eterm(), FLAG_PRINT, true);
        let stamped = state
            .advance_serial(pid(1), pid(1).to_eterm())
            .expect("stamped");
        assert_eq!(stamped.serial, 1);
    }

    #[test]
    fn the_system_tracer_is_the_one_node_global_slot() {
        let mut state = SeqTrace::new();
        assert_eq!(state.set_tracer(Some(pid(3))), None);
        assert_eq!(state.tracer(), Some(pid(3)));
        // `reset_seq_trace` leaves the tracer alone (the C does not touch it either).
        state.reset();
        assert_eq!(state.tracer(), Some(pid(3)));
        assert_eq!(state.set_tracer(None), Some(pid(3)));
        assert_eq!(state.tracer(), None);
    }

    #[test]
    fn the_system_logger_is_the_one_node_global_slot() {
        let mut state = SeqTrace::new();
        // The C initialises the atomic to `am_logger` (`erl_trace.c:339`).
        assert_eq!(state.system_logger(), SystemLogger::Logger);
        // The exchange reports the previous value, like `erts_atomic_xchg_nob`.
        assert_eq!(
            state.set_system_logger(SystemLogger::Pid(pid(4))),
            SystemLogger::Logger
        );
        assert_eq!(state.system_logger(), SystemLogger::Pid(pid(4)));
        assert_eq!(
            state.set_system_logger(SystemLogger::Undefined),
            SystemLogger::Pid(pid(4))
        );
        // `reset_seq_trace` leaves the logger alone (the C does not touch it either).
        state.reset();
        assert_eq!(state.system_logger(), SystemLogger::Undefined);
    }

    #[test]
    fn system_flag_system_logger_takes_the_c_three_values_and_answers_the_previous_one() {
        let mut rt = crate::native::test_support::runtime();
        let system_logger = crate::native::test_support::atom(&mut rt, "system_logger");
        let logger = crate::native::test_support::atom(&mut rt, "logger");
        let undefined = crate::native::test_support::atom(&mut rt, "undefined");
        let caller = rt.self_pid();
        let caller_term = caller.to_eterm();

        // `erlang:system_flag(system_logger, Value)` through the floor's own dispatch.
        fn flag(rt: &mut crate::runtime::Runtime, flag: Eterm, value: Eterm) -> Result<Eterm> {
            crate::native::test_support::call(rt, "erlang", "system_flag", &[flag, value])
        }

        // `logger_proxy:init/1`'s `erlang:system_flag(system_logger, self())`
        // (`lib/kernel/src/logger_proxy.erl:117`): the C's initial `logger` is reported and the
        // pid is stored.
        assert_eq!(
            flag(&mut rt, system_logger, caller_term).expect("flag/2"),
            logger
        );
        assert_eq!(rt.seq_trace().system_logger(), SystemLogger::Pid(caller));
        // Setting a value is idempotent: the second call answers the pid it just stored.
        assert_eq!(
            flag(&mut rt, system_logger, caller_term).expect("flag/2"),
            caller_term
        );
        // `logger_proxy.erl:147`'s `undefined` answers the previous pid...
        assert_eq!(
            flag(&mut rt, system_logger, undefined).expect("flag/2"),
            caller_term
        );
        assert_eq!(rt.seq_trace().system_logger(), SystemLogger::Undefined);
        // ... and setting it again answers `undefined`.
        assert_eq!(
            flag(&mut rt, system_logger, undefined).expect("flag/2"),
            undefined
        );
        // The atom `logger` is accepted too (`erts_set_system_logger`'s first alternative) and
        // answers the previous value.
        assert_eq!(
            flag(&mut rt, system_logger, logger).expect("flag/2"),
            undefined
        );
        assert_eq!(rt.seq_trace().system_logger(), SystemLogger::Logger);

        // Anything else is the C's `THE_NON_VALUE` → `badarg` (`erl_trace.c:2488-2489`), and the
        // state is left untouched. The oracle's own probes: `42` and `{a}`.
        assert!(matches!(
            flag(&mut rt, system_logger, make_small(42)),
            Err(BeamError::BadArg)
        ));
        let tuple = {
            let tag = crate::native::test_support::atom(&mut rt, "a");
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut rt, &mut sink);
            beam_engine::terms::make_tuple(&mut context, &[tag]).expect("a {a}")
        };
        assert!(matches!(
            flag(&mut rt, system_logger, tuple),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            flag(&mut rt, system_logger, NIL),
            Err(BeamError::BadArg)
        ));
        assert_eq!(rt.seq_trace().system_logger(), SystemLogger::Logger);

        // A `Flag` that is not an atom is `badarg`; **another atom** stays the honest typed
        // capability gap (never a fabricated `badarg` for a real OTP flag).
        assert!(matches!(
            flag(&mut rt, make_small(1), caller_term),
            Err(BeamError::BadArg)
        ));
        let scheduling = crate::native::test_support::atom(&mut rt, "scheduling");
        let gap = flag(&mut rt, scheduling, caller_term).expect_err("a capability gap");
        assert!(
            crate::error::unsupported_reason(&gap).is_some(),
            "a real flag is the typed capability gap, got {gap:?}"
        );
    }
}
