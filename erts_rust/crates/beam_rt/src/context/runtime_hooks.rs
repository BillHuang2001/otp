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

//! The runtime's concrete [`RuntimeHooks`] dispatcher — the Stage-5 BIF families.
//!
//! `beam_bifs` registers the whole declared `bif.tab` surface, but a large part of it can
//! only be answered by the *running* runtime: the timer wheel, the code store and process
//! control. Every one of those keys resolves through the **single** defaulted
//! [`RuntimeHooks::call`] seam; with no provider installed the default is the typed
//! `unsupported_bif` capability gap (exit 3). This module is the provider
//! [`RuntimeContext::call_registered`](super::RuntimeContext::call_registered) installs for
//! the keys [`RuntimeContext::uses_runtime_hooks`](super::RuntimeContext) claims.
//!
//! # Served keys (and only these)
//!
//! | Key | Provider |
//! |---|---|
//! | `erlang:start_timer/3,4` | [`crate::timers`] — deliver `{timeout, TRef, Msg}` |
//! | `erlang:send_after/4` | [`crate::timers`] — deliver `Msg` |
//! | `erlang:cancel_timer/2` | [`crate::timers`] — the `/1` answers under the option list |
//! | `erlang:function_exported/3` | the [`CodeServer`](crate::CodeServer) + the BIF registry |
//! | `erlang:link/1`, `erlang:unlink/1` | [`beam_process::ProcessTable`]'s link set |
//! | `erlang:dt_spread_tag/1`, `erlang:dt_restore_tag/1` | the oracle's boolean check (dynamic tracing is compiled out) |
//! | `erlang:statistics/1` | `run_queue` from [`Runtime::run_queue_len`]; `wall_clock` from the process clock |
//!
//! Every **other** `(module, function, arity)` answers the same typed capability gap the
//! defaulted seam does (see [`RuntimeHooksBridge::call`]), so installing this bridge never
//! turns a gap into a fabricated value (workspace `CONTEXT.md` §3).
//!
//! # Documented gaps inside a served key
//!
//! * `{abs, true}` on `start_timer/4`/`send_after/4` is an absolute **Erlang monotonic**
//!   time. The runtime's timer heap is a relative [`Instant`](std::time::Instant) deadline
//!   heap and the monotonic clock's origin lives inside `beam_bifs`
//!   (`beam_bifs/src/time.rs`'s process-global `OnceLock<Instant>`), which no safe API
//!   exposes — so the absolute form is reported as the typed capability gap rather than
//!   approximated by a millisecond-off estimate. `{abs, false}` (the default) IS served.
//! * `statistics/1` serves **only** the keys this runtime can measure for real
//!   (`run_queue`, `wall_clock`, see [`RuntimeHooksBridge::statistics`]); every other key
//!   the oracle accepts is a valid statistics key this runtime cannot answer honestly, so
//!   it reports the typed capability gap. A name that is not a statistics key at all is
//!   the oracle's `badarg`.
//!
//! # Destination semantics (the C `erts_hl_timer`, read-only reference)
//!
//! A pid target is pinned when the timer is scheduled; a registered name is resolved when
//! the timer **fires** (so a name registered in the meantime is found); a dead pid simply
//! receives nothing. Both rules are [`crate::timers::TimerTarget`]'s and are shared with the
//! runtime-native `erlang:send_after/3`.

use beam_alloc::Heap;
use beam_bifs::RuntimeHooks;
use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_process::{PidTarget, ProcessId, ProcessTable};
use beam_term::prelude::{atom_val, is_atom, is_small, make_small, signed_value};
use beam_term::{AtomTable, Eterm};

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::error;
use crate::runtime::Runtime;
use crate::timers::{self, TimerTarget};

/// The [`RuntimeHooks`] provider over the running runtime.
///
/// It holds `&mut Runtime` (the timer heap, the reference id space, the code server) plus
/// the calling process's table and pid — the same pair of borrows
/// [`ProcessBridge`](super::ProcessBridge) resolves, which is why it is its own mutually
/// exclusive arm: the table is the caller-supplied one on a scheduler pass (the runtime's
/// own has been moved out) and the runtime's own otherwise.
pub(super) struct RuntimeHooksBridge<'a> {
    /// The runtime: the timer heap, the reference id space and the code server.
    pub(super) runtime: &'a mut Runtime,
    /// The caller-supplied process table, if any (see [`ProcessBridge::table_mut`]).
    pub(super) table: Option<&'a mut ProcessTable>,
    /// The pid the call runs as (the sender of every timer delivery and the subject of
    /// `link/1`/`unlink/1`).
    pub(super) self_pid: ProcessId,
}

impl<'a> RuntimeHooksBridge<'a> {
    /// The process table a pid resolves against: the caller-supplied one when there is one,
    /// else the runtime's own.
    fn table_mut(&mut self) -> &mut ProcessTable {
        match self.table {
            Some(ref mut table) => &mut **table,
            None => &mut self.runtime.processes,
        }
    }

    /// `erlang:start_timer/3` — the `/4` form with the empty option list.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::start_timer`]'s errors.
    fn start_timer_3(&mut self, heap: &mut Heap, args: &[Eterm]) -> Result<Eterm> {
        self.start_timer(heap, args, false)
    }

    /// `erlang:start_timer/4` — `{abs, false}` is served, `{abs, true}` is the typed gap.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-pid/non-atom destination, a negative or unrepresentable time, a
    /// malformed option list; a typed capability gap for `{abs, true}`.
    fn start_timer_4(&mut self, heap: &mut Heap, args: &[Eterm]) -> Result<Eterm> {
        let options = *args.get(3).ok_or(BeamError::BadArg)?;
        let abs = self.timer_options(options)?;
        self.start_timer(heap, args, abs)
    }

    /// The shared body of `erlang:start_timer/3,4`: `Time`, `Dest`, `Msg` (and whether the
    /// time is absolute) deliver `{timeout, TRef, Msg}` and answer `TRef`.
    ///
    /// # Errors
    ///
    /// As for [`Self::start_timer_4`]; plus [`BeamError::BadArg`] when an argument is
    /// missing.
    fn start_timer(&mut self, heap: &mut Heap, args: &[Eterm], abs: bool) -> Result<Eterm> {
        let delay = self.timer_delay_checked(heap, args, abs)?;
        let target = timers::timer_target(*args.get(1).ok_or(BeamError::BadArg)?)?;
        let message = *args.get(2).ok_or(BeamError::BadArg)?;
        let reference = self.runtime.refs_mut().next().to_eterm(heap)?;
        let timeout = heap.tuple(&[
            atom_term(self.runtime.code_server.atoms(), "timeout")?,
            reference,
            message,
        ])?;
        self.schedule(delay, target, timeout, reference, heap)
    }

    /// `erlang:send_after/4` — the `/3` form plus the `{abs, Bool}` option list, delivering
    /// the bare `Msg` (the oracle's `send_after` never wraps it: only `start_timer` sends
    /// `{timeout, TRef, Msg}`).
    ///
    /// # Errors
    ///
    /// As for [`Self::start_timer_4`].
    fn send_after_4(&mut self, heap: &mut Heap, args: &[Eterm]) -> Result<Eterm> {
        let options = *args.get(3).ok_or(BeamError::BadArg)?;
        let abs = self.timer_options(options)?;
        let delay = self.timer_delay_checked(heap, args, abs)?;
        let target = timers::timer_target(*args.get(1).ok_or(BeamError::BadArg)?)?;
        let message = *args.get(2).ok_or(BeamError::BadArg)?;
        let reference = self.runtime.refs_mut().next().to_eterm(heap)?;
        self.schedule(delay, target, message, reference, heap)
    }

    /// `erlang:cancel_timer/2` — cancelling with the option list `[{async, B}, {info, B}]`.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`), and exactly what this answers:
    ///
    /// * the first argument must be a **reference this runtime minted** (a timer reference,
    ///   a `make_ref/0` reference, a monitor reference) — anything else is `badarg`;
    /// * `Options` must be a list of `{async, boolean()}` / `{info, boolean()}` tuples, in
    ///   any order, **last occurrence winning**; anything else (a bare atom such as `[info]`,
    ///   a non-list, an improper list, `[{info, 1}]`, an unknown key) is `badarg`;
    /// * with `{async, true}` — or with `{info, false}`, the default — the answer is the atom
    ///   `ok`, whether or not the timer was still pending;
    /// * with `{info, true}` and no `async`, the remaining milliseconds when the timer was
    ///   still pending, else the atom `false` (a fired/cancelled timer, and any other
    ///   reference, are `false`).
    ///
    /// # Errors
    ///
    /// `badarg` for a non-reference first argument or a malformed option list.
    fn cancel_timer_2(&mut self, heap: &mut Heap, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // A timer reference, or a reference this runtime minted for something else
        // (`make_ref/0`, a monitor) — anything else is not a reference and is `badarg`, the
        // same test `erlang:cancel_timer/1` (the runtime-native floor) applies.
        if !self.runtime.timers.is_timer_ref(term)
            && !self.runtime.references.contains_key(&term)
        {
            return Err(BeamError::BadArg);
        }
        let options = *args.get(1).ok_or(BeamError::BadArg)?;
        let (async_, info) = self.cancel_options(options)?;
        let cancelled = self.runtime.timers_mut().cancel(term);
        if async_ || !info {
            return atom_term(self.runtime.code_server.atoms(), "ok");
        }
        match cancelled {
            Some(remaining) => {
                // Whole milliseconds, truncated — the oracle answers the same unit.
                let millis = i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX);
                Ok(make_small(millis))
            }
            None => atom_term(self.runtime.code_server.atoms(), "false"),
        }
        .map_err(|err| {
            let _ = heap;
            err
        })
    }

    /// `erlang:function_exported/3` — whether a **loaded** module exports `Function/Arity`.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): the module and the function must both be atoms
    /// and the arity a small integer (anything else is `badarg` — a negative arity is *not*
    /// an error, it simply answers `false`); the answer is `true` iff the module is loaded
    /// and exports the function **or** the `M:F/A` is a built-in BIF
    /// (`erts/emulator/beam/bif.c:4409-4424`, which calls `erts_find_function` then
    /// `erts_is_builtin`). It never **autoloads**: an unloaded module answers `false`.
    ///
    /// The loaded-module half is the [`CodeServer`](crate::CodeServer)'s real module set —
    /// the 22 preloaded `.beam`s plus everything resolved on demand — and the built-in half
    /// is the live `beam_bifs` [`registry`](crate::Runtime::registry), so the answer is
    /// always about *this* runtime's real state (never a fabricated export). Like
    /// `erlang:module_loaded/1`, a module this runtime has not loaded answers `false` where
    /// the oracle — which boots a much larger module set — may answer `true`; that is a
    /// module-load-state difference, not a wrong answer about a loaded module.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-atom module/function or a non-integer arity.
    fn function_exported(&mut self, heap: &mut Heap, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let module_term = *args.first().ok_or(BeamError::BadArg)?;
        let function_term = *args.get(1).ok_or(BeamError::BadArg)?;
        let arity_term = *args.get(2).ok_or(BeamError::BadArg)?;
        if !is_atom(module_term) || !is_atom(function_term) || !is_small(arity_term) {
            return Err(BeamError::BadArg);
        }
        let module = atoms
            .name(atom_val(module_term))
            .ok_or(BeamError::BadArg)?;
        let function = atoms
            .name(atom_val(function_term))
            .ok_or(BeamError::BadArg)?;
        // A negative arity can never match an export or a built-in, so it answers `false`
        // (the oracle does the same — it is a valid small integer, not an error).
        let arity = u32::try_from(signed_value(arity_term)).unwrap_or(u32::MAX);
        let exported = self
            .runtime
            .registry
            .contains(module, function, arity)
            || self
                .runtime
                .code_server
                .module(module)
                .is_some_and(|loaded| module_exports(loaded, function, arity));
        let _ = heap;
        atom_term(atoms, if exported { "true" } else { "false" })
    }

    /// `erlang:link/1`: link the calling process to `Target`, answering `true`.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): `Target` must be a pid — anything else is
    /// `badarg`; a pid answers `true`, including `self()` and a pid no process backs (the
    /// oracle discards a link to a non-existent process and still reports success, exactly
    /// as the C's `erts_proc_lookup_raw` miss does). A **remote** pid is a link to another
    /// node, which needs distribution (contract A.5) — the typed capability gap rather than
    /// a fabricated success.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-pid; the typed gap for a remote pid, for a caller with no process
    /// in the table, and for any link-set failure the table reports.
    fn link(&mut self, args: &[Eterm], unlink: bool) -> Result<Eterm> {
        let target = local_pid(*args.first().ok_or(BeamError::BadArg)?, "link")?;
        let self_pid = self.self_pid;
        if !self.table_mut().contains(self_pid) {
            return Err(error::no_process(self_pid));
        }
        let outcome = if unlink {
            self.table_mut().unlink_term(self_pid, target)
        } else {
            self.table_mut().link_term(self_pid, target)
        };
        match outcome {
            Ok(()) => atom_term(self.runtime.code_server.atoms(), "true"),
            // The target pid is not in the table: the oracle treats a pid no process backs
            // as a successful no-op link (the signal/`{'EXIT',…}` simply never arrives), so
            // this is the oracle's semantics rather than a swallowed failure. The *caller*
            // is known to exist (checked above), so this can only be the target.
            Err(BeamError::TermNotFound) => {
                atom_term(self.runtime.code_server.atoms(), "true")
            }
            Err(other) => Err(other),
        }
    }

    /// `erlang:dt_spread_tag/1` — the dynamic-tracing tag spreader, a **boolean check**.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): the argument must be the atom `true` **or** the
    /// atom `false` — anything else is `badarg` — and the answer is the atom `true`. With
    /// dynamic tracing compiled out (the oracle's own build, and this runtime's only mode)
    /// the C's `dt_spread_tag_1` does exactly that and nothing else
    /// (`erts/emulator/beam/bif.c:6080-6112`): it validates the boolean and answers `true`;
    /// only under `USE_VM_PROBES` would it instead answer the previous `{Flags, Tag}` pair.
    /// `lib/kernel/src/file.erl`'s `call/2` brackets every file-server round trip with
    /// `dt_spread_tag(true)` / `dt_restore_tag(X)`, so serving this pair is what lets the
    /// `file:` / `code:where_is_file` / `.app`-read boot path run at all.
    ///
    /// # Errors
    ///
    /// `badarg` for anything but the atoms `true`/`false`.
    fn dt_spread_tag(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let tag = *args.first().ok_or(BeamError::BadArg)?;
        // Both booleans are accepted; the validation is the whole body (see above).
        let _ = boolean_value(atoms, tag)?;
        atom_term(atoms, "true")
    }

    /// `erlang:dt_restore_tag/1` — the dynamic-tracing tag restorer, a **`true` check**.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`): the argument must be the atom `true` —
    /// anything else, `false` **included**, is `badarg` — and the answer is the atom
    /// `true`. That is `dt_restore_tag_1`'s no-`USE_VM_PROBES` body
    /// (`erts/emulator/beam/bif.c:6162-6167`); under `USE_VM_PROBES` the argument is
    /// instead the `{Flags, Tag}` tuple [`Self::dt_spread_tag`] returned, which this build
    /// never produces. The two are always called in that pairing by
    /// `file:call/2`.
    ///
    /// # Errors
    ///
    /// `badarg` for anything but the atom `true`.
    fn dt_restore_tag(&self, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let tag = *args.first().ok_or(BeamError::BadArg)?;
        // `boolean_value` reads `true` as `true` and `false` as `false`; only the former is
        // accepted, so `false` is the oracle's `badarg`.
        if !boolean_value(atoms, tag)? {
            return Err(BeamError::BadArg);
        }
        atom_term(atoms, "true")
    }

    /// `erlang:statistics/1` — the counters this runtime can answer **honestly** from its
    /// own real state.
    ///
    /// Oracle-verified (OTP 29/`erts-17.1`). The two keys served, and why each figure is a
    /// real measurement rather than a placeholder:
    ///
    /// * `run_queue` → the total length of the runnable queues, as a small integer. This is
    ///   [`Runtime::run_queue_len`], the length of the runtime's **one** real run queue
    ///   (`beam_process`'s `Scheduler`). The oracle sums every scheduler's queue
    ///   (`erts_run_queues_len(...)`); this runtime is single-threaded, so it is the same
    ///   quantity over the same kind of queue — both answer `0` once the node is idle.
    /// * `wall_clock` → `{TotalMilliseconds, MillisecondsSinceLastCall}`, read from the
    ///   process monotonic clock ([`std::time::Instant`]). The oracle's relationship holds
    ///   exactly: the first read answers the same value for both elements, and every later
    ///   read answers the delta since the previous one. This is what `c:uptime/0` reads.
    ///
    /// **Documented divergence (the epoch, never the clock).** The oracle's `wall_clock`
    /// total is measured from emulator start; this runtime anchors it at the **first**
    /// `statistics(wall_clock)` read, because no earlier anchor is reachable from this seam
    /// — the runtime's other monotonic origin lives inside `beam_bifs` (`src/time.rs`) and
    /// exposes no reader. The numbers are therefore real elapsed time with a different
    /// zero, exactly the class of divergence as `erlang:processes/0` ("an answer that reads
    /// *this* runtime's own state").
    ///
    /// Every **other** key the oracle accepts is a genuine statistics key this runtime
    /// cannot measure — `reductions`/`exact_reductions` need a node-global **cumulative**
    /// reduction counter (this runtime's counters are per-process and reset by every
    /// scheduler slice, so summing them would be a made-up total), `runtime` needs CPU time,
    /// `context_switches` a context-switch counter, `garbage_collection` a collector,
    /// `scheduler_wall_time`/`scheduler_wall_time_all` per-scheduler CPU time,
    /// `microstate_accounting` the MSACC machinery, `io` I/O byte counters and `run_queues`
    /// / `active_tasks*` / `*_run_queue_lengths*` the per-scheduler queue lists — so each
    /// answers the typed capability gap rather than a fabricated number (workspace
    /// `CONTEXT.md` §3). A name that is **not** a statistics key at all is the oracle's
    /// `badarg` (`erl_bif_info.c:4333`), which is reproduced rather than reported as a gap.
    ///
    /// # Errors
    ///
    /// `badarg` for a non-atom or an unknown key; the typed capability gap for a valid key
    /// this runtime cannot measure.
    fn statistics(&self, heap: &mut Heap, atoms: &AtomTable, args: &[Eterm]) -> Result<Eterm> {
        let key = *args.first().ok_or(BeamError::BadArg)?;
        if !is_atom(key) {
            return Err(BeamError::BadArg);
        }
        // A name the shared table does not hold cannot name a statistics key, so it is the
        // oracle's `badarg` (never a substituted key).
        let name = atoms.name(atom_val(key)).ok_or(BeamError::BadArg)?;
        match name {
            "run_queue" => Ok(make_small(self.runtime.run_queue_len() as i64)),
            "wall_clock" => {
                let (total, since) = wall_clock_millis();
                heap.tuple(&[make_small(total), make_small(since)])
            }
            other if STATISTICS_KEYS.contains(&other) => Err(error::unsupported(&format!(
                "erlang:statistics/1: the `{other}` counter is not implemented"
            ))),
            _ => Err(BeamError::BadArg),
        }
    }

    /// Schedules `message` for `target` at `delay` from now under the reference `reference`,
    /// and answers `reference`.
    ///
    /// # Errors
    ///
    /// Propagates the heap's failure when the reference term cannot be built — impossible
    /// here (it was already built by the caller before scheduling).
    fn schedule(
        &mut self,
        delay: std::time::Duration,
        target: TimerTarget,
        message: Eterm,
        reference: Eterm,
        heap: &mut Heap,
    ) -> Result<Eterm> {
        let _ = heap;
        let from = self.self_pid.to_eterm();
        self.runtime
            .timers_mut()
            .schedule(delay, target, from, message, reference);
        Ok(reference)
    }

    /// The time argument of a timer BIF, validated by the shared
    /// [`crate::timers::timer_delay`] and then rejected as the typed capability gap when it
    /// is an **absolute** time (`{abs, true}`, see the module docs).
    ///
    /// The validation runs **first**: a bad time is the oracle's `badarg` even when the
    /// options also ask for the absolute form.
    ///
    /// # Errors
    ///
    /// As for [`crate::timers::timer_delay`], plus the typed gap for `abs`.
    fn timer_delay_checked(
        &self,
        heap: &mut Heap,
        args: &[Eterm],
        abs: bool,
    ) -> Result<std::time::Duration> {
        let _ = heap;
        let delay = timers::timer_delay(*args.first().ok_or(BeamError::BadArg)?)?;
        if abs {
            return Err(error::unsupported(
                "erlang:start_timer/4, send_after/4 with {abs,true}: the runtime has no \
                 Erlang monotonic clock anchor to interpret an absolute time",
            ));
        }
        Ok(delay)
    }

    /// Parses the `start_timer/4` / `send_after/4` option list, answering whether the time is
    /// absolute.
    ///
    /// Only `{abs, boolean()}` is accepted (the oracle's documented option set for these two
    /// BIFs); a non-list, an improper list, a non-2-tuple element, an unknown key and a
    /// non-boolean value are all `badarg`.
    ///
    /// # Errors
    ///
    /// `badarg` for a malformed option list.
    fn timer_options(&self, options: Eterm) -> Result<bool> {
        let elements = terms::list_elements(options).map_err(|_| BeamError::BadArg)?;
        let mut abs = false;
        for option in elements {
            let (key, value) = option_pair(self.runtime.code_server.atoms(), option)?;
            if key != "abs" {
                return Err(BeamError::BadArg);
            }
            abs = boolean_value(self.runtime.code_server.atoms(), value)?;
        }
        Ok(abs)
    }

    /// Parses the `cancel_timer/2` option list, answering `(async, info)`.
    ///
    /// The oracle accepts `{async, boolean()}` and `{info, boolean()}` in any order with the
    /// **last** occurrence of each key winning (verified against OTP 29/`erts-17.1`).
    ///
    /// # Errors
    ///
    /// `badarg` for a malformed option list, exactly as [`Self::timer_options`].
    fn cancel_options(&self, options: Eterm) -> Result<(bool, bool)> {
        let elements = terms::list_elements(options).map_err(|_| BeamError::BadArg)?;
        let mut async_ = false;
        let mut info = false;
        for option in elements {
            let (key, value) = option_pair(self.runtime.code_server.atoms(), option)?;
            let flag = boolean_value(self.runtime.code_server.atoms(), value)?;
            match key.as_str() {
                "async" => async_ = flag,
                "info" => info = flag,
                _ => return Err(BeamError::BadArg),
            }
        }
        Ok((async_, info))
    }
}

impl RuntimeHooks for RuntimeHooksBridge<'_> {
    fn call(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        module: &str,
        function: &str,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match (module, function, args.len()) {
            ("erlang", "start_timer", 3) => self.start_timer_3(heap, args),
            ("erlang", "start_timer", 4) => self.start_timer_4(heap, args),
            ("erlang", "send_after", 4) => self.send_after_4(heap, args),
            ("erlang", "cancel_timer", 2) => self.cancel_timer_2(heap, args),
            ("erlang", "function_exported", 3) => self.function_exported(heap, atoms, args),
            ("erlang", "link", 1) => self.link(args, false),
            ("erlang", "unlink", 1) => self.link(args, true),
            ("erlang", "dt_spread_tag", 1) => self.dt_spread_tag(atoms, args),
            ("erlang", "dt_restore_tag", 1) => self.dt_restore_tag(atoms, args),
            ("erlang", "statistics", 1) => self.statistics(heap, atoms, args),
            // Unreachable: `RuntimeContext::uses_runtime_hooks` gates this bridge to exactly
            // the keys above. Reporting the same typed capability gap the defaulted seam
            // reports keeps an ungated key honest rather than fabricated (workspace
            // `CONTEXT.md` §3).
            _ => Err(error::unsupported(&format!(
                "{module}:{function}/{}",
                args.len()
            ))),
        }
    }
}

/// Every key the oracle's `erlang:statistics/1` accepts beyond the two this runtime serves
/// (`erts/emulator/beam/erl_bif_info.c:4169-4332`, plus the `ERTS_ENABLE_MSACC`-gated
/// `microstate_accounting`).
///
/// The list exists to tell a **valid** key this runtime cannot measure (the honest typed
/// capability gap, [`RuntimeHooksBridge::statistics`]) from a name that is not a statistics
/// key at all (the oracle's `badarg`) — the only two answers, since a fabricated figure is
/// never acceptable (workspace `CONTEXT.md` §3).
const STATISTICS_KEYS: &[&str] = &[
    "scheduler_wall_time",
    "scheduler_wall_time_all",
    "total_active_tasks",
    "total_run_queue_lengths",
    "total_active_tasks_all",
    "total_run_queue_lengths_all",
    "active_tasks",
    "run_queue_lengths",
    "active_tasks_all",
    "run_queue_lengths_all",
    "microstate_accounting",
    "context_switches",
    "garbage_collection",
    "reductions",
    "exact_reductions",
    "runtime",
    "io",
    "run_queues",
];

thread_local! {
    /// The wall-clock anchor and the previous `statistics(wall_clock)` read, in one slot.
    ///
    /// A `thread_local` (rather than a `Runtime` field) because this seam may not touch the
    /// runtime's layout, and it is sound because the runtime is single-threaded (`!Send`,
    /// the same rationale `src/context/trace.rs`'s slot uses). The `None` state is the
    /// first read: the anchor is then *now*, which is the runtime's documented wall-clock
    /// epoch (see [`RuntimeHooksBridge::statistics`]).
    static WALL_CLOCK: Cell<Option<(Instant, Instant)>> = const { Cell::new(None) };
}

/// The wall clock in whole milliseconds: `(since the anchor, since the previous read)`.
///
/// `Instant` is the platform's monotonic clock, so the figures are real elapsed time and
/// never go backwards. The first read returns `(0, 0)` — the oracle's first read returns
/// the same value for both elements too, only measured from emulator start rather than from
/// this anchor (see [`RuntimeHooksBridge::statistics`]).
fn wall_clock_millis() -> (i64, i64) {
    let now = Instant::now();
    WALL_CLOCK.with(|slot| {
        // An unanchored slot is its own anchor and its own "previous read".
        let (base, last) = slot.get().unwrap_or((now, now));
        slot.set(Some((base, now)));
        (
            whole_millis(now.saturating_duration_since(base)),
            whole_millis(now.saturating_duration_since(last)),
        )
    })
}

/// `duration` in whole milliseconds, saturated at `i64::MAX` (a `Duration` can exceed an
/// `i64` of milliseconds only after ~292 million years, but the conversion must not trap).
fn whole_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// A `{Key, Value}` option tuple, or `badarg` for anything else.
///
/// # Errors
///
/// `badarg` for a non-tuple, a tuple of another arity, or a key that is not an atom of the
/// caller's shared table.
fn option_pair(atoms: &AtomTable, option: Eterm) -> Result<(String, Eterm)> {
    if terms::tuple_arity(option).map_err(|_| BeamError::BadArg)? != 2 {
        return Err(BeamError::BadArg);
    }
    let key = terms::tuple_element(option, 0).map_err(|_| BeamError::BadArg)?;
    let value = terms::tuple_element(option, 1).map_err(|_| BeamError::BadArg)?;
    if !is_atom(key) {
        return Err(BeamError::BadArg);
    }
    let key = atoms.name(atom_val(key)).ok_or(BeamError::BadArg)?;
    Ok((key.to_string(), value))
}

/// The boolean the atom `value` names, or `badarg`.
///
/// # Errors
///
/// `badarg` for anything but the atoms `true`/`false` — the oracle rejects `{abs, 1}` and
/// `{info, 1}` alike.
fn boolean_value(atoms: &AtomTable, value: Eterm) -> Result<bool> {
    if !is_atom(value) {
        return Err(BeamError::BadArg);
    }
    match atoms.name(atom_val(value)) {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(BeamError::BadArg),
    }
}

/// The local pid `term` names, or the oracle's rejection of it.
///
/// # Errors
///
/// `badarg` for anything that is not a pid; the typed capability gap for a **remote** pid
/// (distribution, contract A.5, is not implemented) — never a fabricated local pid.
fn local_pid(term: Eterm, what: &str) -> Result<Eterm> {
    match PidTarget::from_eterm(term) {
        Ok(PidTarget::Local(_)) => Ok(term),
        Ok(PidTarget::Remote(_)) => Err(error::unsupported(&format!(
            "erlang:{what}/1 on a remote pid: distribution is not implemented"
        ))),
        Err(_) => Err(BeamError::BadArg),
    }
}

/// Whether the loaded `module` exports `function/arity`.
///
/// The module's own atom table names its `ExpT` entries (the loader interns them there),
/// so the comparison is by **name**, never by index — a caller's atom index is a different
/// namespace.
fn module_exports(module: &beam_loader::Module, function: &str, arity: u32) -> bool {
    module.exports.iter().any(|export| {
        export.arity == arity && module.atom_name(export.function) == Some(function)
    })
}

/// An interned atom's term, resolved by **name** through `atoms`.
///
/// The runtime's boot seed (see [`CORE_ATOMS`](crate::boot::CORE_ATOMS)) holds every atom
/// this bridge emits (`timeout`, `true`, `false`, `ok`), so the lookup always succeeds.
///
/// # Errors
///
/// The typed capability gap for a name the shared table does not hold — never a substituted
/// atom index.
fn atom_term(atoms: &AtomTable, name: &str) -> Result<Eterm> {
    match atoms.lookup(name) {
        Some(atom) => Ok(beam_term::make_atom(atom)),
        None => Err(error::unsupported(&format!(
            "runtime hook: the atom `{name}` is not interned"
        ))),
    }
}
