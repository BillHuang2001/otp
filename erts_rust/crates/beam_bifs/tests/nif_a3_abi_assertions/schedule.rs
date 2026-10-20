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

//! §1 — `enif_schedule_nif`'s **continuation trampoline**, asserted in-process.
//!
//! The oracle transcript behind this module is `../HANDOFF_A3_NIF_HOST.md` §1 and its replay is
//! `../nif_a3_oracle_replay.rs`: `sched_sum(2,3)` → `{scheduled,5}`, `sched_last_ret()` → `0`
//! (`THE_NON_VALUE`), `sched_loop(100000)` with an unchanged `stack_size`, `sched_raise()` →
//! `error:boom`, `sched_ghost_name(99)` → `{ghost,99}` and `sched_dirty(7)` → `{dirty,7}` with the
//! continuation's `enif_thread_type()` answering `2`.
//!
//! Each continuation below is the fixture's own, re-expressed as a Rust NIF entry point, and each
//! assertion pins one row of that table. The trampoline's observables are `thread_local!` statics, so
//! the single test that drives them is safe to run beside the other two tests in this binary.
//!
//! The row that needs a word of explanation is `sched_last_ret()`: the C answers `0` on every path
//! because the value reaches the caller later, through the emulator's re-run of the trap. This crate
//! folds that re-run into the **outermost** call (the substitution `beam_abi`'s `nif::enif::schedule`
//! module documents), so the outermost call answers the chain's value — and, for a raise, the pending
//! reason term, i.e. the very word `enif_raise_exception` returned. A **nested** schedule does answer
//! the C's `0`, because there the C's `THE_NON_VALUE` is exactly what the continuation receives back.

use std::cell::Cell;

use libc::c_int;

use beam_abi::common::{ERL_DIRTY_JOB_CPU_BOUND, ERL_DIRTY_JOB_IO_BOUND};
use beam_abi::nif::enif::exception::enif_raise_exception;
use beam_abi::nif::enif::get::enif_get_int;
use beam_abi::nif::enif::make::{enif_make_atom, enif_make_int};
use beam_abi::nif::enif::process::enif_schedule_nif;
use beam_abi::nif::enif::schedule::{in_continuation, non_value_term};
use beam_abi::nif::enif::threads::enif_thread_type;
use beam_abi::nif::types::{
    ErlNifEnv, ERL_NIF_THR_DIRTY_CPU_SCHEDULER, ERL_NIF_THR_DIRTY_IO_SCHEDULER,
    ERL_NIF_THR_UNDEFINED,
};
use beam_abi::{nif_from_eterm, NifEnv, NifHost, ERL_NIF_TERM};
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, make_atom, make_small, Eterm};

thread_local! {
    /// How often [`cont_sum`] was entered.
    static SUM_RUNS: Cell<usize> = const { Cell::new(0) };

    /// `(argc, argv pointer, argv[0] raw, argv[1] raw)` [`cont_sum`] was entered with.
    static SUM_SEEN: Cell<(c_int, usize, usize, usize)> = const { Cell::new((0, 0, 0, 0)) };

    /// `(nested return word, in_continuation(), the outer frame's argv pointer)` — what
    /// [`cont_outer`] saw of its own nested schedule.
    static OUTER_SEEN: Cell<(usize, bool, usize)> = const { Cell::new((usize::MAX, false, 0)) };

    /// `(argc, argv pointer, argv[0] value)` — what [`cont_inner`] (the nested continuation) saw.
    static NESTED_SEEN: Cell<(c_int, usize, i64)> = const { Cell::new((0, 0, i64::MIN)) };

    /// How many continuations [`cont_chain`] entered (one per trampoline iteration).
    static CHAIN_ENTRIES: Cell<usize> = const { Cell::new(0) };

    /// How many times the continuation a raise must suppress ever ran.
    static TAIL_RUNS: Cell<usize> = const { Cell::new(0) };

    /// `(enif_raise_exception's return word, the boom atom's word)` — recorded by the raising
    /// continuation, so the test can pin the trampoline's return word to both.
    static RAISE_SEEN: Cell<(usize, usize)> = const { Cell::new((usize::MAX, usize::MAX)) };
}

/// A [`NifHost`] that records what the trampoline asks of it.
///
/// [`NifHost::nif_yield_point`] counts the preemption points (the oracle's fairness seam), and
/// [`NifHost::schedule_nif`] is overridden **on purpose**: the *legacy* hook is kept for source
/// compatibility only, so a test must be able to prove it is never consulted.
struct SchedHost {
    heap: Heap,
    atoms: AtomTable,
    /// Every name the entry point interned, in order.
    interned: Vec<String>,
    /// How many times the trampoline called [`NifHost::nif_yield_point`].
    yields: usize,
    /// Every call the legacy [`NifHost::schedule_nif`] hook received — it must stay empty.
    legacy_hook_calls: Vec<(String, c_int, usize)>,
}

impl SchedHost {
    fn new() -> SchedHost {
        SchedHost {
            heap: Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            interned: Vec::new(),
            yields: 0,
            legacy_hook_calls: Vec::new(),
        }
    }
}

impl NifHost for SchedHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        self.interned.push(name.to_string());
        Some(make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }

    fn nif_yield_point(&mut self, _flags: c_int) {
        self.yields += 1;
    }

    fn schedule_nif(
        &mut self,
        fun_name: &str,
        flags: c_int,
        _fptr: unsafe extern "C" fn(*mut ErlNifEnv, c_int, *const ERL_NIF_TERM) -> ERL_NIF_TERM,
        argc: usize,
    ) -> Option<Eterm> {
        // Reached only if `enif_schedule_nif` still delegated to the legacy hook.
        self.legacy_hook_calls
            .push((fun_name.to_string(), flags, argc));
        Some(make_atom(self.atoms.intern("legacy-hook")))
    }
}

/// The oracle's `sched_sum/2` continuation: records exactly what it was entered with and answers
/// `argv[0] + argv[1]` (so `2, 3` ⇒ `5`, the number behind the oracle's `{scheduled,5}`).
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_sum(
    env: *mut ErlNifEnv,
    argc: c_int,
    argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    SUM_RUNS.with(|c| c.set(c.get() + 1));
    let mut a: c_int = 0;
    let mut b: c_int = 0;
    let mut seen = (argc, argv as usize, usize::MAX, usize::MAX);
    if argc == 2 && !argv.is_null() {
        // SAFETY: the caller's contract — `argv` is readable for `argc` (== 2) terms.
        let words = [unsafe { *argv }, unsafe { *argv.add(1) }];
        seen.2 = words[0].raw();
        seen.3 = words[1].raw();
        // SAFETY: `env` is live and both words are the terms just read.
        let ok = unsafe { enif_get_int(env, words[0], &mut a) } != 0
            && unsafe { enif_get_int(env, words[1], &mut b) } != 0;
        if !ok {
            // Not a number: answer 0 and let the test's assertions on `SUM_SEEN` catch it.
            a = 0;
            b = 0;
        }
    }
    SUM_SEEN.with(|c| c.set(seen));
    // SAFETY: `env` is live (the caller's contract); a small integer needs no heap.
    unsafe { enif_make_int(env, a + b) }
}

/// Schedules [`cont_inner`] from **inside** a continuation with an argument vector that is a local
/// array of this stack frame — the C idiom `ERL_NIF_TERM next[1]; … return
/// enif_schedule_nif(…, next);` — and records the raw word it got back.
///
/// After the nested call returns, the array is overwritten with a *different* value: the storage is
/// this frame's, so a trampoline that borrowed the pointer instead of copying the words would let
/// [`cont_inner`] read `99`.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_outer(
    env: *mut ErlNifEnv,
    _argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    let inside = in_continuation();
    // SAFETY: `env` is live (the caller's contract); a small integer needs no heap.
    let mut next = [unsafe { enif_make_int(env, 11) }];
    let next_ptr = next.as_mut_ptr();
    // SAFETY: `env` is live, the name is NUL-terminated, `next` is readable for 1 term.
    let returned = unsafe {
        enif_schedule_nif(env, c"cont_inner".as_ptr(), 0, Some(cont_inner), 1, next_ptr)
    };
    // Scribble over the frame the nested call named (see the doc above). Volatile so the write
    // really happens: a plain assignment to a dead local would let the optimiser drop the evidence
    // this line exists to provide.
    // SAFETY: `next_ptr` is `next`'s own storage, writable for one term; `env` is live.
    unsafe { core::ptr::write_volatile(next_ptr, enif_make_int(env, 99)) };
    OUTER_SEEN.with(|c| c.set((returned.raw(), inside, next_ptr as usize)));
    returned
}

/// The continuation [`cont_outer`] scheduled: answers `argv[0]`, which must be the value the
/// **copied** snapshot holds (`11`), not what the dead frame was scribbled with (`99`).
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_inner(
    env: *mut ErlNifEnv,
    argc: c_int,
    argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    let mut n: c_int = -1;
    if argc == 1 && !argv.is_null() {
        // SAFETY: `argv` is readable for `argc` (== 1) terms; `env` is live.
        let _ = unsafe { enif_get_int(env, *argv, &mut n) };
    }
    NESTED_SEEN.with(|c| c.set((argc, argv as usize, i64::from(n))));
    // SAFETY: `env` is live; a small integer needs no heap.
    unsafe { enif_make_int(env, n) }
}

/// The oracle's `sched_loop/1` continuation: counts down `argv[0]` by *rescheduling itself* with a
/// fresh local argument array, and answers the number of continuation entries once it reaches zero.
/// The chain is walked without growing the Rust stack, which is what the oracle's 100 000-entry run
/// (unchanged `stack_size`) proves.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_chain(
    env: *mut ErlNifEnv,
    argc: c_int,
    argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    let mut n: c_int = -1;
    if argc == 1 && !argv.is_null() {
        // SAFETY: `argv` is readable for `argc` (== 1) terms; `env` is live.
        let _ = unsafe { enif_get_int(env, *argv, &mut n) };
    }
    let entries = CHAIN_ENTRIES.with(|c| {
        let next = c.get() + 1;
        c.set(next);
        next
    });
    if n <= 0 {
        // SAFETY: `env` is live; a small integer needs no heap.
        return unsafe { enif_make_int(env, entries as c_int) };
    }
    // SAFETY: as above.
    let mut next = [unsafe { enif_make_int(env, n - 1) }];
    // SAFETY: `env` is live, the name is NUL-terminated, `next` is readable for 1 term. The tail
    // position is the C idiom: the nested answer (`THE_NON_VALUE`) is this continuation's answer.
    unsafe {
        enif_schedule_nif(
            env,
            c"cont_chain".as_ptr(),
            0,
            Some(cont_chain),
            1,
            next.as_mut_ptr(),
        )
    }
}

/// Raises `boom` **and** records a reschedule: the C's `execute_nif` checks `freason` before it looks
/// at anything else (`erl_nif.c:3527-3541`), so the raise is terminal and [`cont_tail`] must not run.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_raise_then_reschedule(
    env: *mut ErlNifEnv,
    _argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    // SAFETY: `env` is live and the name is a NUL-terminated literal.
    let boom = unsafe { enif_make_atom(env, c"boom".as_ptr()) };
    // SAFETY: `env` is live.
    let raised = unsafe { enif_raise_exception(env, boom) };
    // SAFETY: `env` is live; a small integer needs no heap.
    let mut next = [unsafe { enif_make_int(env, 1) }];
    // SAFETY: `env` is live, the name is NUL-terminated, `next` is readable for 1 term.
    let _ = unsafe {
        enif_schedule_nif(
            env,
            c"cont_tail".as_ptr(),
            0,
            Some(cont_tail),
            1,
            next.as_mut_ptr(),
        )
    };
    RAISE_SEEN.with(|c| c.set((raised.raw(), boom.raw())));
    raised
}

/// The continuation a raise must suppress.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_tail(
    env: *mut ErlNifEnv,
    _argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    TAIL_RUNS.with(|c| c.set(c.get() + 1));
    // SAFETY: `env` is live; a small integer needs no heap.
    unsafe { enif_make_int(env, 0) }
}

/// Answers the `enif_thread_type()` its continuation saw — the dirty-scheduler observable the
/// `a3_schedule` fixture probes (`{dirty,7}` with `enif_thread_type() == 2`).
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_thread_type(
    env: *mut ErlNifEnv,
    _argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    // SAFETY: `enif_thread_type` takes no pointers and never panics.
    let ty = unsafe { enif_thread_type() };
    // SAFETY: `env` is live; a small integer needs no heap.
    unsafe { enif_make_int(env, ty) }
}

/// The oracle's `sched_sum(2,3)` → `{scheduled,5}`, and everything else §1 promises.
///
/// The sections below are one test on purpose: they share the (thread-local, so parallel-safe)
/// trampoline observables and each one needs its own host/env pair to keep the counters meaningful.
#[test]
fn enif_schedule_nif_runs_its_continuation_to_completion() {
    // -----------------------------------------------------------------------------------------
    // A. The continuation runs, exactly once, with the caller's own argv, and its value is the
    //    call's result. `fun_name` is interned, never resolved (the oracle's `sched_ghost_name`).
    // -----------------------------------------------------------------------------------------
    SUM_RUNS.with(|c| c.set(0));
    SUM_SEEN.with(|c| c.set((0, 0, 0, 0)));
    let mut host = SchedHost::new();
    let mut env = NifEnv::new(&mut host);
    let argv = [nif_from_eterm(make_small(2)), nif_from_eterm(make_small(3))];
    let argv_ptr = argv.as_ptr();
    // SAFETY: `env` is live, `fun_name` is NUL-terminated (and names nothing, which is legal: the C
    // only interns it), `fptr` is a real NIF entry point and `argv` is readable for 2 terms.
    let returned = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"ghost_name".as_ptr(),
            0,
            Some(cont_sum),
            2,
            argv_ptr,
        )
    };
    assert_eq!(
        returned.raw(),
        nif_from_eterm(make_small(5)).raw(),
        "argv[0] + argv[1] = 2 + 3 is the call's result (the oracle's {{scheduled,5}})"
    );
    assert_eq!(
        env.pending_exception(),
        None,
        "a chain that produced a value raises nothing"
    );
    assert_eq!(
        SUM_RUNS.with(Cell::get),
        1,
        "the continuation ran exactly once"
    );
    assert_eq!(
        SUM_SEEN.with(Cell::get),
        (
            2,
            argv_ptr as usize,
            nif_from_eterm(make_small(2)).raw(),
            nif_from_eterm(make_small(3)).raw()
        ),
        "the continuation sees the caller's own argc, argv POINTER (uncopied) and words"
    );
    assert!(
        host.interned.iter().any(|name| name == "ghost_name"),
        "fun_name is interned — the C never resolves it against the module's function table"
    );
    assert!(
        host.legacy_hook_calls.is_empty(),
        "the legacy NifHost::schedule_nif hook is never consulted any more"
    );
    assert_eq!(
        host.yields, 1,
        "one trampoline iteration (one continuation) is one yield point"
    );

    // -----------------------------------------------------------------------------------------
    // B. A nested schedule answers the C's THE_NON_VALUE, the outermost call still yields the
    //    chain's final value, and the nested continuation's arguments outlived their frame.
    // -----------------------------------------------------------------------------------------
    OUTER_SEEN.with(|c| c.set((usize::MAX, false, 0)));
    NESTED_SEEN.with(|c| c.set((0, 0, i64::MIN)));
    let mut host = SchedHost::new();
    let mut env = NifEnv::new(&mut host);
    let outer_argv = [nif_from_eterm(make_small(7))];
    // SAFETY: as above — live env, NUL-terminated name, real entry point, 1 readable term.
    let returned = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_outer".as_ptr(),
            0,
            Some(cont_outer),
            1,
            outer_argv.as_ptr(),
        )
    };
    assert_eq!(
        returned.raw(),
        nif_from_eterm(make_small(11)).raw(),
        "the outermost call yields the chain's final value (the nested continuation's argv[0])"
    );
    let (nested_returned, inside, outer_frame_ptr) = OUTER_SEEN.with(Cell::get);
    assert_eq!(
        nested_returned,
        non_value_term().raw(),
        "a nested enif_schedule_nif answers the C's THE_NON_VALUE (raw word 0)"
    );
    assert!(inside, "the nested schedule really was inside a continuation");
    let (nested_argc, nested_argv_ptr, nested_value) = NESTED_SEEN.with(Cell::get);
    assert_eq!(nested_argc, 1, "the nested continuation's own argc");
    assert_eq!(
        nested_value, 11,
        "the nested continuation read the value the outer frame named, not its later scribble (99)"
    );
    assert_ne!(
        nested_argv_ptr, outer_frame_ptr,
        "the nested call's argv is a copy the trampoline owns, not the dead frame's storage"
    );

    // -----------------------------------------------------------------------------------------
    // C. A self-rescheduling chain: one yield point per iteration, constant Rust stack (5 entries
    //    for n = 4, i.e. n = 4,3,2,1,0).
    // -----------------------------------------------------------------------------------------
    CHAIN_ENTRIES.with(|c| c.set(0));
    let mut host = SchedHost::new();
    let mut env = NifEnv::new(&mut host);
    let chain_argv = [nif_from_eterm(make_small(4))];
    // SAFETY: as above.
    let returned = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_chain".as_ptr(),
            0,
            Some(cont_chain),
            1,
            chain_argv.as_ptr(),
        )
    };
    assert_eq!(CHAIN_ENTRIES.with(Cell::get), 5, "five continuation entries");
    assert_eq!(
        returned.raw(),
        nif_from_eterm(make_small(5)).raw(),
        "the chain's end answers the entry count, and the outermost call answers it"
    );
    assert_eq!(
        host.yields, 5,
        "the host's yield point runs once per trampoline iteration, so a 5-entry chain has 5"
    );
    assert_eq!(
        env.pending_exception(),
        None,
        "a chain is not a raise, however long it is"
    );

    // -----------------------------------------------------------------------------------------
    // D. A raise inside a continuation ends the chain: the env's slot holds the reason, a recorded
    //    reschedule never runs, and the trampoline answers the pending reason word.
    //
    //    The C answers THE_NON_VALUE here too (the emulator re-runs the trap and reads `freason`);
    //    this crate folds that re-run into the outermost call and therefore answers the reason — the
    //    same word `enif_raise_exception` returned inside the continuation. That is the documented
    //    substitution (`beam_abi`'s `nif::enif::schedule` module docs), not a fabricated value.
    // -----------------------------------------------------------------------------------------
    TAIL_RUNS.with(|c| c.set(0));
    RAISE_SEEN.with(|c| c.set((usize::MAX, usize::MAX)));
    let mut host = SchedHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live, the name is NUL-terminated, `argc` is 0 so a null `argv` is the legal
    // "no arguments" form.
    let returned = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_raise_then_reschedule".as_ptr(),
            0,
            Some(cont_raise_then_reschedule),
            0,
            core::ptr::null(),
        )
    };
    let pending = env
        .pending_exception()
        .expect("the raise left the env's exception slot set — that is what the host acts on");
    let (raise_word, boom_word) = RAISE_SEEN.with(Cell::get);
    assert_eq!(
        raise_word, boom_word,
        "enif_raise_exception returns its reason verbatim"
    );
    assert_eq!(
        returned.raw(),
        raise_word,
        "the trampoline answers the pending reason word — the same word enif_raise_exception \
         returns (the C answers THE_NON_VALUE here; see this section's comment)"
    );
    assert_eq!(
        nif_from_eterm(pending).raw(),
        boom_word,
        "the env's pending reason is the boom atom"
    );
    assert_eq!(
        host.atom_name(pending).as_deref(),
        Some("boom"),
        "and it names correctly"
    );
    assert_eq!(
        TAIL_RUNS.with(Cell::get),
        0,
        "a raise takes precedence over a recorded reschedule: the chain is over"
    );

    // -----------------------------------------------------------------------------------------
    // E. Dirty-job flags: the continuation reports the C's dirty scheduler type, and the surrounding
    //    answer is restored afterwards.
    // -----------------------------------------------------------------------------------------
    // SAFETY: `enif_thread_type` takes no pointers.
    let before = unsafe { enif_thread_type() };
    assert_eq!(
        before, ERL_NIF_THR_UNDEFINED,
        "this file never runs on an enif_thread_create'd thread, so the baseline is the C's 0"
    );
    let mut host = SchedHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live, the name is NUL-terminated, `argc` is 0 with a null `argv`.
    let cpu = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_thread_type".as_ptr(),
            ERL_DIRTY_JOB_CPU_BOUND,
            Some(cont_thread_type),
            0,
            core::ptr::null(),
        )
    };
    assert_eq!(
        cpu.raw(),
        nif_from_eterm(make_small(i64::from(ERL_NIF_THR_DIRTY_CPU_SCHEDULER))).raw(),
        "ERL_DIRTY_JOB_CPU_BOUND ⇒ enif_thread_type() is 2 inside the continuation"
    );
    // SAFETY: as above.
    let io = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_thread_type".as_ptr(),
            ERL_DIRTY_JOB_IO_BOUND,
            Some(cont_thread_type),
            0,
            core::ptr::null(),
        )
    };
    assert_eq!(
        io.raw(),
        nif_from_eterm(make_small(i64::from(ERL_NIF_THR_DIRTY_IO_SCHEDULER))).raw(),
        "ERL_DIRTY_JOB_IO_BOUND ⇒ enif_thread_type() is 3 inside the continuation"
    );
    // SAFETY: as above.
    assert_eq!(
        unsafe { enif_thread_type() },
        before,
        "the dirty marker is scoped to the continuation, so the caller's answer is restored"
    );

    // -----------------------------------------------------------------------------------------
    // F. The C's refusals: `argc > MAX_ARG` (255) and a flags bit outside the two dirty bits are
    //    `badarg`, they raise, and they run nothing.
    // -----------------------------------------------------------------------------------------
    SUM_RUNS.with(|c| c.set(0));
    let mut host = SchedHost::new();
    let mut env = NifEnv::new(&mut host);
    let badarg = host
        .intern_atom("badarg")
        .expect("the test host interns unconditionally");
    let probe_argv = [nif_from_eterm(make_small(1))];
    // SAFETY: `argc` 256 is refused before `fun_name` is even read, so only the env is used.
    let too_many = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"must_not_intern".as_ptr(),
            0,
            Some(cont_sum),
            256,
            probe_argv.as_ptr(),
        )
    };
    assert_eq!(
        too_many.raw(),
        nif_from_eterm(badarg).raw(),
        "argc > MAX_ARG (255) is the C's enif_make_badarg"
    );
    assert_eq!(
        env.pending_exception(),
        Some(badarg),
        "a refusal is RAISED, not merely returned"
    );
    env.clear();

    // SAFETY: `flags` 4 is outside ERL_DIRTY_JOB_{CPU,IO}_BOUND, so it is refused before anything runs.
    let bad_flags = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_sum".as_ptr(),
            4,
            Some(cont_sum),
            2,
            probe_argv.as_ptr(),
        )
    };
    assert_eq!(
        bad_flags.raw(),
        nif_from_eterm(badarg).raw(),
        "a flags bit outside the two dirty bits is badarg too"
    );
    assert_eq!(
        env.pending_exception(),
        Some(badarg),
        "and it raises as well"
    );
    assert_eq!(
        SUM_RUNS.with(Cell::get),
        0,
        "no refusal may run a continuation"
    );
    assert!(
        !host.interned.iter().any(|name| name == "must_not_intern"),
        "argc is validated before fun_name is interned (the C's check order)"
    );
    assert!(
        host.legacy_hook_calls.is_empty(),
        "no refusal consults the legacy hook either"
    );
}
