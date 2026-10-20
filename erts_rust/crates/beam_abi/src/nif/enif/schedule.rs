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

//! `enif_schedule_nif`'s **success path** — the continuation trampoline (workspace contract **A.3**).
//!
//! [`run`] is the entry [`enif_schedule_nif`](super::process::enif_schedule_nif) calls once it has
//! validated its arguments. It owns the whole lifetime of a scheduled call: it *runs* the
//! continuation, follows a self-rescheduling chain to its end, and hands the chain's final value back
//! to the NIF call that started it. Nothing here consults
//! [`NifHost::schedule_nif`](crate::nif::host::NifHost::schedule_nif) — that hook is kept only for
//! source compatibility (see its own documentation).
//!
//! # What the oracle does (measured, OTP 29.1.1 / erts-17.1)
//!
//! The measurements below come from the real C fixture
//! `beam_bifs/tests/fixtures/a3_schedule.c` run against the live system `erl`; they are the
//! specification this module implements (the full transcript is in
//! `beam_bifs/HANDOFF_A3_NIF_HOST.md` §1).
//!
//! | probe | oracle answer |
//! |---|---|
//! | `sched_sum(2,3)` — schedules `cont_sum`, which builds `{scheduled, N+M}` from **its** `argv` | `{scheduled,5}` |
//! | `sched_last_ret()` — the raw word `enif_schedule_nif` returned | **`0`** (`THE_NON_VALUE`), on **every** path, the refusals included |
//! | `sched_loop(100000)` — a self-rescheduling chain | `{done,100001}`, `process_info(_, stack_size)` unchanged |
//! | `sched_raise()` — a continuation calling `enif_raise_exception(boom)` | `error:boom` |
//! | `sched_bad_flags()` / `sched_too_many_args()` / `enif_make_badarg` inside a continuation | `error:badarg` |
//! | `sched_dirty(7)` (`ERL_NIF_DIRTY_JOB_CPU_BOUND`) | `{dirty,7}`, and the continuation's `enif_thread_type()` is `2` (vs `1` for `flags == 0`) |
//! | `sched_ghost_name(99)` — `fun_name` naming nothing | `{ghost,99}` — the name is interned, never resolved |
//!
//! # The three-way outcome
//!
//! Ground truth `erts/emulator/beam/erl_nif.c:3561-3604` (`enif_schedule_nif`) and
//! `erts/emulator/beam/erl_nfunc_sched.c:66-146` (`erts_nfunc_schedule`): the C *always* returns
//! `(ERL_NIF_TERM)THE_NON_VALUE` (`erl_nif.c:399`) and the continuation's value reaches the caller
//! later, through `execute_nif` (`erl_nif.c:3512-3558`) once the emulator re-runs the trap. "Rescheduled"
//! and "raised" are both expressed as that same non-value word and are told apart only by the
//! process's `freason`, so a term sentinel cannot model the seam. [`Outcome`] makes the three
//! possibilities explicit instead.
//!
//! **The one substitution this seam creates.** The emulator's re-run is folded into the *outermost*
//! `enif_schedule_nif` call here: the trampoline loops until the chain yields a value or raises, and
//! that value is what the `enif_*` layer returns. The Erlang-visible result is identical
//! (`sched_sum(2,3)` → `{scheduled,5}`, `sched_loop(100000)` → `{done,100001}`), but a probe that
//! exposes the **raw word** `enif_schedule_nif` returned sees the continuation's value where the C
//! sees `0` (`sched_last_ret()` answers `0` on the oracle). The nested case *does* answer the C's
//! `0`: a continuation that schedules the next continuation gets `THE_NON_VALUE` back, exactly as in
//! the C (`[`run`]`'s nested branch).
//!
//! **The outermost call's return word, case by case:**
//!
//! | chain ends with | outermost `enif_schedule_nif` returns |
//! |---|---|
//! | a value | that value (the C returns `0` here — the documented substitution above) |
//! | a raise, i.e. the env's pending exception is set | the pending reason, as a NIF term — the same word [`enif_raise_exception`](super::exception::enif_raise_exception) returns for that reason (`enif_make_badarg` likewise returns the `badarg` atom) |
//! | nothing runnable (a null `env`) | `THE_NON_VALUE` (`0`); the `enif_*` layer refuses such a call before it gets here |
//!
//! A raise is what the *host* acts on: it reads the env's exception slot (this crate never clears
//! it), so the returned word is informational there. The word is kept consistent with
//! `enif_raise_exception`/`enif_make_badarg` rather than fabricated as a value.
//!
//! # Constant Rust stack (the chain is iterative)
//!
//! A self-rescheduling chain must not grow the stack: the oracle runs a 10 000 000-entry chain and a
//! 100 000-entry one with an unchanged `stack_size`, because a reschedule is a *trap* the emulator
//! loops on, not a call. Recursing per reschedule would blow the native stack. The mechanism here is
//! a thread-local pending-continuation slot plus a nesting counter:
//!
//! * the **outermost** [`run`] owns the loop and never recurses;
//! * a **nested** `enif_schedule_nif` (a continuation scheduling the next one — [`DEPTH`] > 0)
//!   *records* its [`Continuation`] in [`PENDING`] and returns the C's `THE_NON_VALUE` (`0`), so the
//!   Rust stack depth is one continuation call no matter how long the chain is.
//!
//! # Nothing is copied (except where the C copies)
//!
//! The C copies the *new* `argv` into the x-register array (`erl_nfunc_sched.c:126-130`) and runs the
//! continuation on the *same* env. For the **outermost** schedule that array is the calling NIF's own
//! `argv`, alive for the whole chain — the trampoline runs inside that call — so [`Continuation`]
//! carries the caller's `argv` **pointer** and passes it straight to the function pointer: the words
//! the continuation reads are the words the scheduling call passed in.
//!
//! A **nested** schedule is the case the C's copy exists for: `ERL_NIF_TERM next[1]; … return
//! enif_schedule_nif(…, next);` names a *local* array of the continuation's frame, which is gone by
//! the time the next continuation is entered. [`Continuation::own_args`] therefore takes the same copy
//! the C takes, into a buffer the continuation owns, so a recorded call's arguments outlive the frame
//! that named them (reading the dead frame would be undefined behaviour, not a substitution).
//!
//! # Preemption: the yield point
//!
//! A chain would otherwise hold the calling thread for its whole length. The oracle is fair — with
//! `+S 1:1` an unrelated process ran 3 000 033 `receive … after 0` iterations *while* a 3 000 000-entry
//! chain ran — so the host is given a yield point once per trampoline iteration
//! ([`NifHost::nif_yield_point`](crate::nif::host::NifHost::nif_yield_point), defaulted to a no-op).
//! A host that does not implement it keeps the old "the chain holds the thread" behaviour; one that
//! does can preempt (and, in a real scheduler, reschedule) there.
//!
//! # Dirty-job flags
//!
//! No dirty scheduler pool exists in this runtime (`beam_rt` reports `dirty_scheduler_support: 0`), so
//! a dirty-flagged continuation runs **on the calling thread**. The value, `argv` and env semantics
//! are identical to the oracle's; the *thread identity* is not. What is reproduced is the observable
//! the fixture probes: [`enif_thread_type`](super::threads::enif_thread_type) answers the C's
//! dirty-scheduler type for the duration of a dirty-flagged continuation — `2`
//! (`ERL_NIF_THR_DIRTY_CPU_SCHEDULER`) for `ERL_NIF_DIRTY_JOB_CPU_BOUND` and `3`
//! (`ERL_NIF_THR_DIRTY_IO_SCHEDULER`) for `ERL_NIF_DIRTY_JOB_IO_BOUND`. **If both bits are set the
//! CPU type is reported**, deterministically (the C would pick whichever pool the job landed on).
//! Outside a dirty-flagged continuation `enif_thread_type` behaves exactly as before.
//!
//! # Raise attribution (the seam boundary)
//!
//! The C attributes a raise inside a continuation to the **scheduling NIF**, with the *original* MFA
//! and the *original* `argv` — never the continuation's (`execute_nif` sets `EXF_RESTORE_NFUNC`,
//! `erl_nif.c:3534-3541`, restored by `handle_error` from `nep->argv`/`nep->mfa` saved at the first
//! schedule, `erl_nfunc_sched.c:106-134`; measured `{a3_schedule,sched_raise,[],[]}`). Reproducing
//! that needs a stacktrace head, which is the **engine's** to build — this crate cannot construct one
//! and deliberately does not fabricate frames. The trampoline therefore records the boundary here and
//! nothing more: the raise leaves the env's pending exception set (the reason the host raises) and
//! ends the chain, so the host's own error path attributes it to the NIF call in progress — which
//! *is* the scheduling NIF.
//!
//! # Safety
//!
//! [`run`] takes the C ABI's raw pointers. Every function here documents its own contract; the
//! continuations this module invokes are C entry points whose pointer validity, `argc`/`argv`
//! agreement and env lifetime are the caller's.

use core::cell::{Cell, RefCell};

use libc::c_int;

use super::*;

use crate::common::{ERL_DIRTY_JOB_CPU_BOUND, ERL_DIRTY_JOB_IO_BOUND};
use crate::nif::types::{
    ErlNifEnv, ERL_NIF_THR_DIRTY_CPU_SCHEDULER, ERL_NIF_THR_DIRTY_IO_SCHEDULER,
};

/// The C NIF entry-point type a continuation is entered through (`ERL_NIF_TERM (*)(ErlNifEnv*, int,
/// const ERL_NIF_TERM[])` — the `fptr` argument of `enif_schedule_nif`).
pub type NifContinuation = unsafe extern "C" fn(
    *mut ErlNifEnv,
    c_int,
    *const ERL_NIF_TERM,
) -> ERL_NIF_TERM;

/// Everything one continuation call needs: the live env, the entry point and the arguments the
/// scheduling call passed for it.
///
/// # Why the words are not always the caller's
///
/// The **outermost** schedule borrows the caller's argument vector verbatim: it is the NIF call's own
/// `argv`, which lives on the caller's env heap (or in the register array it was entered with) and
/// stays valid until the whole chain has finished — the trampoline runs inside that call, so nothing
/// has to be copied and the words the continuation reads are the words the scheduling call passed in.
///
/// A **nested** schedule (`ERL_NIF_TERM next[1]; … return enif_schedule_nif(…, next);` — the C idiom
/// the oracle fixture uses) is different: its `argv` is a local array of the *continuation's* stack
/// frame, which is gone by the time the trampoline enters the next continuation. The C is safe there
/// because `schedule()` copies the new `argv` into the process's x-register array immediately
/// (`erl_nfunc_sched.c:126-130`); this seam reproduces exactly that copy, into a buffer the
/// [`Continuation`] owns, so the recorded call's arguments outlive the frame that named them.
/// (Reading the dead frame instead — "the bits are usually still there" — would be undefined
/// behaviour, not a substitution.)
#[derive(Debug)]
pub struct Continuation {
    /// The env the whole chain runs on (the C's `nep->env`; one env per scheduled call).
    env: *mut ErlNifEnv,
    /// The continuation's entry point.
    fptr: NifContinuation,
    /// The argument count the continuation is entered with (`0..=MAX_ARG`).
    argc: c_int,
    /// The argument vector the continuation is entered with: the caller's own pointer, or (once
    /// [`Continuation::own_args`] has run) the snapshot below.
    argv: *const ERL_NIF_TERM,
    /// The dirty-job flags the schedule was made with (`0` = the normal scheduler).
    flags: c_int,
    /// The owned argument snapshot a **recorded** (nested) continuation needs, so `argv` stays valid
    /// after the scheduling call's frame is gone. `None` for the outermost schedule (nothing copied).
    snapshot: Option<Box<[ERL_NIF_TERM]>>,
}

impl Continuation {
    /// Record one continuation call, borrowing the caller's argument vector.
    pub const fn new(
        env: *mut ErlNifEnv,
        fptr: NifContinuation,
        argc: c_int,
        argv: *const ERL_NIF_TERM,
        flags: c_int,
    ) -> Continuation {
        Continuation {
            env,
            fptr,
            argc,
            argv,
            flags,
            snapshot: None,
        }
    }

    /// Take a copy of the argument words, so this continuation stays valid after the frame that
    /// named them is gone — the C's `memcpy` into the x-register array (`erl_nfunc_sched.c:126-130`).
    /// Called for a **nested** schedule; the outermost one borrows instead (see the type docs).
    fn own_args(&mut self) {
        let count = if self.argc > 0 { self.argc as usize } else { 0 };
        if count == 0 {
            // `argc == 0`: no words to copy, and a dangling-but-aligned pointer is the correct
            // "zero-length" vector for the entry point.
            let empty: Box<[ERL_NIF_TERM]> = Box::new([]);
            self.argv = empty.as_ptr();
            self.snapshot = Some(empty);
            return;
        }
        // SAFETY: `argv` is readable for `argc` terms — `enif_schedule_nif` checked that a null
        // `argv` is only allowed with `argc == 0`, and the pointer's readability for `argc` terms is
        // the C ABI's own contract (`# Safety` on `run`).
        let words: Box<[ERL_NIF_TERM]> =
            unsafe { core::slice::from_raw_parts(self.argv, count) }.into();
        self.argv = words.as_ptr();
        self.snapshot = Some(words);
    }

    /// The env the chain runs on.
    pub const fn env(&self) -> *mut ErlNifEnv {
        self.env
    }

    /// The continuation's entry point.
    pub const fn fptr(&self) -> NifContinuation {
        self.fptr
    }

    /// The argument count the continuation is entered with.
    pub const fn argc(&self) -> c_int {
        self.argc
    }

    /// The argument vector the continuation is entered with (the caller's, or the owned snapshot).
    pub fn argv(&self) -> *const ERL_NIF_TERM {
        self.argv
    }

    /// The dirty-job flags this schedule was made with.
    pub const fn flags(&self) -> c_int {
        self.flags
    }

    /// Whether this continuation owns a snapshot of its arguments (i.e. it was recorded nested).
    pub const fn copies_args(&self) -> bool {
        self.snapshot.is_some()
    }
    /// Enter the continuation once and classify what it did (see [`Outcome`]).
    ///
    /// The env's pending exception is consulted **after** the call and takes precedence over a
    /// recorded reschedule, so a continuation that both raised and rescheduled is a raise — the
    /// chain ends (the C's `execute_nif` checks `freason` before it looks at anything else,
    /// `erl_nif.c:3527-3541`).
    ///
    /// # Safety
    ///
    /// `self.env` must be a live env (with its host alive) for the duration of the call, and `argv`
    /// must be readable for `argc` terms — the C ABI's own contract for a NIF entry point.
    unsafe fn invoke(&self) -> Outcome {
        // Publish the thread type this continuation should report for its whole duration (the C
        // enters a dirty-flagged continuation from a dirty scheduler, so it *is* that thread type
        // there). Saved and restored so an enclosing continuation — or the plain NIF call the chain
        // started from — keeps its own answer, which is what makes `enif_thread_type` unchanged
        // outside a dirty-flagged continuation.
        let saved_dirty = DIRTY.with(|d| d.replace(dirty_thread_type(self.flags)));
        let saved_depth = DEPTH.with(|d| {
            let depth = d.get();
            d.set(depth + 1);
            depth
        });
        // SAFETY: the caller's contract — a live env and `argv` readable for `argc` terms. The
        // continuation may re-enter `enif_schedule_nif` (which records and returns the non-value),
        // and any `enif_*` call it makes recovers this same live env from `self.env`.
        let term = unsafe { (self.fptr)(self.env, self.argc, self.argv) };
        DEPTH.with(|d| d.set(saved_depth));
        DIRTY.with(|d| d.set(saved_dirty));
        // SAFETY: as above; `env_mut` only re-borrows the env that is still live.
        let raised = match unsafe { env_mut(self.env) } {
            Some(e) => e.pending_exception().is_some(),
            // Unreachable: `run` refuses a null env before any continuation is entered; a caller
            // that violates `invoke`'s contract gets the value the continuation returned and no
            // fabricated raise.
            None => false,
        };
        if raised {
            return Outcome::Raised;
        }
        match PENDING.with(|p| p.borrow_mut().take()) {
            Some(next) => Outcome::Reschedule(next),
            // The C's value is a NIF term; the outcome carries the internal term so the seam's
            // `Eterm` model is used at the boundary (the conversion is the crate's identity map).
            None => Outcome::Value(t(term)),
        }
    }
}

/// What one continuation entry did — the explicit model of the C's term-sentinel + `freason` pair
/// (`erl_nif.c:3527-3541`).
#[derive(Debug)]
pub enum Outcome {
    /// The continuation produced the call's value.
    Value(Eterm),
    /// The continuation scheduled the next one; the chain continues with it.
    Reschedule(Continuation),
    /// The continuation raised: the env's pending exception is set and the chain is over. The
    /// reason is on the env (this crate's raise convention), not in the outcome.
    Raised,
}

// ---------------------------------------------------------------------------------------------
// The trampoline's thread-local state
// ---------------------------------------------------------------------------------------------
//
// All three are thread-local because a continuation runs synchronously on the thread that entered
// `enif_schedule_nif` — there is no dirty-scheduler hop in this runtime (see the module docs), so
// "the chain running on this thread" is the whole state a trampoline needs.

thread_local! {
    /// The continuation a **nested** `enif_schedule_nif` recorded for the running trampoline.
    ///
    /// Set by [`run`]'s nested branch, taken by [`Continuation::invoke`]'s classification. Last
    /// record wins: a continuation that schedules twice before returning leaves the second one (the
    /// C's `schedule()` likewise overwrites the process's saved schedule state,
    /// `erl_nfunc_sched.c:106-138`).
    static PENDING: RefCell<Option<Continuation>> = const { RefCell::new(None) };

    /// How many continuations are executing on this thread right now.
    ///
    /// `0` means "no trampoline is running a continuation", i.e. an `enif_schedule_nif` called with
    /// this value is the **outermost** one and owns the loop; anything else is a nested schedule,
    /// which records and returns the C's `THE_NON_VALUE`.
    static DEPTH: Cell<usize> = const { Cell::new(0) };

    /// The dirty-job flags of the continuation currently executing on this thread (`None` = a
    /// normal, `flags == 0` schedule — so [`enif_thread_type`](super::threads::enif_thread_type)
    /// keeps its ordinary answer).
    static DIRTY: Cell<Option<c_int>> = const { Cell::new(None) };
}

/// The C's `THE_NON_VALUE` (`erl_term.h:376-381`) as a NIF term — the word `0`.
///
/// It is what a **nested** `enif_schedule_nif` returns, and what the C returns on *every* path of the
/// entry point. It is not a term.
pub const fn non_value_term() -> ERL_NIF_TERM {
    ERL_NIF_TERM::from_raw(Eterm::non_value().raw())
}

/// The `enif_thread_type` answer a continuation scheduled with `flags` must report for its duration,
/// or `None` when it must keep the thread's ordinary answer.
///
/// **Both dirty bits set reports `ERL_NIF_THR_DIRTY_CPU_SCHEDULER`**, deterministically (see the
/// module docs).
const fn dirty_thread_type(flags: c_int) -> Option<c_int> {
    if flags & ERL_DIRTY_JOB_CPU_BOUND != 0 {
        Some(ERL_NIF_THR_DIRTY_CPU_SCHEDULER)
    } else if flags & ERL_DIRTY_JOB_IO_BOUND != 0 {
        Some(ERL_NIF_THR_DIRTY_IO_SCHEDULER)
    } else {
        None
    }
}

/// The dirty-scheduler type the continuation executing on this thread must report, if any.
///
/// The trampoline's half of the `enif_thread_type` fix: the `enif_*` layer consults this **first**
/// and only falls through to the thread's own type when it is `None`.
pub(crate) fn current_dirty_thread_type() -> Option<c_int> {
    DIRTY.with(Cell::get)
}

/// Whether a chain is currently running a continuation on this thread (a test/diagnostic query).
pub fn in_continuation() -> bool {
    DEPTH.with(Cell::get) > 0
}

/// Run a scheduled NIF call to completion and answer its value.
///
/// Called by [`enif_schedule_nif`](super::process::enif_schedule_nif) after it has validated
/// `argc`/`flags`/`fun_name`/`fptr`/`argv`. `env` is the whole chain's environment (the C's
/// `nep->env`), `flags` the dirty-job flags of *this* schedule and `argv` the caller's argument
/// vector (read, never copied).
///
/// * **Outermost** (no continuation is running on this thread): loops — yield point, pending
///   exception check, invoke, classify — until the chain produces a value or raises, and returns that
///   value (see the module docs for the exact return word in each case). Constant Rust stack.
/// * **Nested** (a continuation is running): records the continuation for the running trampoline and
///   returns the C's `THE_NON_VALUE` (`0`) to its C caller.
///
/// A `null` `env` runs nothing and answers `THE_NON_VALUE`; the `enif_*` layer refuses such a call
/// with `badarg` before reaching this function, so that answer is only a defensive "nothing ran",
/// never a fabricated value.
///
/// # Safety
///
/// `env` must be null or a live env whose host is alive for the whole chain, and `argv` must be null
/// (only with `argc == 0`) or readable for `argc` terms. The continuation `fptr` must be a valid NIF
/// entry point with that exact `argc`/`argv` agreement.
pub unsafe fn run(
    env: *mut ErlNifEnv,
    flags: c_int,
    fptr: NifContinuation,
    argc: c_int,
    argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    let mut first = Continuation::new(env, fptr, argc, argv, flags);
    // A nested schedule: record it and hand the C's non-value back. The running trampoline picks the
    // continuation up and loops, so the Rust stack depth stays at one continuation call. The
    // arguments are copied here — and only here — because a nested schedule names them with the
    // continuation's own stack frame (see [`Continuation`]).
    if in_continuation() {
        first.own_args();
        PENDING.with(|p| *p.borrow_mut() = Some(first));
        return non_value_term();
    }
    if unsafe { env_mut(env) }.is_none() {
        // Nothing can run without a live env; answer the non-value rather than a value.
        return non_value_term();
    }
    let mut current = first;
    loop {
        // (1) A raise is TERMINAL and takes precedence over a recorded reschedule: the C's
        // `execute_nif` checks `freason` before anything else (`erl_nif.c:3527-3541`).
        match unsafe { env_mut(env) } {
            Some(e) => {
                if let Some(reason) = e.pending_exception() {
                    // Drop a continuation the raising call recorded — the chain is over.
                    PENDING.with(|p| p.borrow_mut().take());
                    return n(reason);
                }
                // (2) The host's preemption point, once per trampoline iteration: a chain must not
                // own the thread for its whole length (see the module docs).
                e.host().nif_yield_point(current.flags);
            }
            // Unreachable: the null env was refused above.
            None => return non_value_term(),
        }
        // (3) Enter the continuation. `invoke` brackets it with the nesting counter (so a nested
        // `enif_schedule_nif` records) and the dirty thread-type marker.
        // SAFETY: this function's contract — a live env and `argv` readable for `argc` terms.
        match unsafe { current.invoke() } {
            Outcome::Value(value) => return n(value),
            // The loop's first step returns the pending reason (it is set: that is what `Raised`
            // means), so a raise ends the chain.
            Outcome::Raised => {}
            Outcome::Reschedule(next) => current = next,
        }
    }
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod tests;
