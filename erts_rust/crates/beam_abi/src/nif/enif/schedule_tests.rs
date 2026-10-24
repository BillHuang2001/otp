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
//! Path-included tests for [`super`] — the `enif_schedule_nif` continuation trampoline.
//!
//! The continuations are real `unsafe extern "C"` NIF entry points (a C fixture cannot be used here:
//! the trampoline is a *host-side* mechanism, and what it must be pinned to is the measured oracle
//! transcript of `beam_bifs/tests/fixtures/a3_schedule.c` — see `HANDOFF_A3_NIF_HOST.md` §1). Each
//! test drives them through the exported [`enif_schedule_nif`], exactly as a NIF `.so` would.
//!
//! Kept in a sibling file so `schedule.rs` stays under the workspace's ~1000-line convention; included
//! via `#[path]` as a **child** of `schedule`, so `use super::*;` still reaches the module's items.

use super::*;

use core::cell::Cell;

use libc::c_char;

use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, make_atom, make_small, NIL};

use crate::nif::enif::exception::enif_raise_exception;
use crate::nif::enif::get::enif_get_int;
use crate::nif::enif::make::enif_make_int;
use crate::nif::enif::process::{enif_schedule_nif, MAX_ARG};
use crate::nif::enif::threads::enif_thread_type;
use crate::nif::host::{NifEnv, NifHost};
use crate::sync;

// ---------------------------------------------------------------------------------------------
// Observability: what each continuation saw (thread-local, so parallel tests never interfere)
// ---------------------------------------------------------------------------------------------

thread_local! {
    /// `(argc, argv pointer, argv[0] raw, argv[1] raw)` the `cont_sum` continuation was entered with.
    static SUM_SEEN: Cell<(c_int, usize, usize, usize)> = const { Cell::new((0, 0, 0, 0)) };

    /// `(nested return word, in_continuation())` recorded by the outer continuation.
    static OUTER_SEEN: Cell<(usize, bool)> = const { Cell::new((usize::MAX, false)) };

    /// How many continuation entries the chain has made (`cont_chain`).
    static CHAIN_ENTRIES: Cell<usize> = const { Cell::new(0) };

    /// How many times the continuation a raise must suppress ever ran (`cont_tail`).
    static TAIL_RUNS: Cell<usize> = const { Cell::new(0) };

    /// How many continuation entries ran at all, across every continuation (`cont_counting_*`).
    static ENTRIES: Cell<usize> = const { Cell::new(0) };
}

/// The `badarg` atom term the host interns, for comparing a refusal.
fn badarg(host: &mut dyn NifHost) -> Eterm {
    host.intern_atom("badarg").expect("interns unconditionally")
}

// ---------------------------------------------------------------------------------------------
// The test host
// ---------------------------------------------------------------------------------------------

/// A [`NifHost`] that records what the trampoline asks of it.
///
/// `nif_yield_point` counts the preemption points (the oracle's fairness seam); `schedule_nif` is
/// overridden **on purpose** so a test can prove the legacy hook is never consulted any more.
struct SchedHost {
    heap: Heap,
    atoms: AtomTable,
    /// Every name the entry point interned, in order.
    interned: Vec<String>,
    /// How many times the trampoline called [`NifHost::nif_yield_point`].
    yields: usize,
    /// Every call the **legacy** [`NifHost::schedule_nif`] hook received — it must stay empty.
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
        _fptr: NifContinuation,
        argc: usize,
    ) -> Option<Eterm> {
        // Reached only if `enif_schedule_nif` still delegated to the legacy hook.
        self.legacy_hook_calls
            .push((fun_name.to_string(), flags, argc));
        Some(make_atom(self.atoms.intern("legacy-hook")))
    }
}

// ---------------------------------------------------------------------------------------------
// The continuations
// ---------------------------------------------------------------------------------------------

/// `cont_sum`-shaped: records exactly what it was entered with and answers `argv[0] + argv[1]`.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_sum(
    env: *mut ErlNifEnv,
    argc: c_int,
    argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    ENTRIES.with(|c| c.set(c.get() + 1));
    let words: [ERL_NIF_TERM; 2] = if argc == 2 && !argv.is_null() {
        // SAFETY: the caller contract — `argv` is readable for `argc` (== 2) terms.
        [unsafe { *argv }, unsafe { *argv.add(1) }]
    } else {
        [ERL_NIF_TERM::from_raw(usize::MAX); 2]
    };
    SUM_SEEN.with(|c| c.set((argc, argv as usize, words[0].raw(), words[1].raw())));
    let mut n: c_int = 0;
    let mut m: c_int = 0;
    // SAFETY: as above — two readable words at `argv`.
    let ok = unsafe { enif_get_int(env, words[0], &mut n) } != 0
        && unsafe { enif_get_int(env, words[1], &mut m) } != 0;
    if !ok {
        // SAFETY: `env` is live (the caller contract).
        return unsafe { crate::nif::enif::exception::enif_make_badarg(env) };
    }
    // SAFETY: `env` is live; a small int needs no heap.
    unsafe { enif_make_int(env, n + m) }
}

/// `cont_loop`-shaped: counts down `argv[0]` by *rescheduling itself* with a fresh **local** argument
/// array (the C idiom), and answers the number of continuation entries once the counter reaches zero.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_chain(
    env: *mut ErlNifEnv,
    argc: c_int,
    argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    if argc != 1 || argv.is_null() {
        // SAFETY: `env` is live (the caller contract).
        return unsafe { crate::nif::enif::exception::enif_make_badarg(env) };
    }
    let mut n: c_int = 0;
    // SAFETY: `argv` is readable for `argc` (== 1) terms.
    if unsafe { enif_get_int(env, *argv, &mut n) } == 0 {
        // SAFETY: as above.
        return unsafe { crate::nif::enif::exception::enif_make_badarg(env) };
    }
    let entries = CHAIN_ENTRIES.with(|c| {
        let next = c.get() + 1;
        c.set(next);
        next
    });
    if n <= 0 {
        // SAFETY: `env` is live; a small int needs no heap.
        return unsafe { enif_make_int(env, entries as c_int) };
    }
    // A LOCAL array — exactly what `a3_schedule.c`'s `cont_loop` does. It is dead once this frame
    // returns, which is why a recorded continuation takes its own copy of the words.
    // SAFETY: `env` is live.
    let mut next = [unsafe { enif_make_int(env, n - 1) }];
    // SAFETY: `env` is live, the name is NUL-terminated, `argv` is readable for `argc` (== 1).
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

/// Schedules `cont_inner` from inside a continuation and records the raw word it got back.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_outer(
    _env: *mut ErlNifEnv,
    _argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    ENTRIES.with(|c| c.set(c.get() + 1));
    let inside = in_continuation();
    // SAFETY: `_env` is live (the caller contract).
    let mut next = [unsafe { enif_make_int(_env, 11) }];
    // SAFETY: `_env` is live, the name is NUL-terminated, `argv` is readable for `argc` (== 1).
    let returned = unsafe {
        enif_schedule_nif(
            _env,
            c"cont_inner".as_ptr(),
            0,
            Some(cont_inner),
            1,
            next.as_mut_ptr(),
        )
    };
    OUTER_SEEN.with(|c| c.set((returned.raw(), inside)));
    returned
}

/// The continuation `cont_outer` scheduled: answers the integer it was handed in `argv[0]` (which came
/// from `cont_outer`'s dead local array), so the value proves the arguments survived the frame.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_inner(
    env: *mut ErlNifEnv,
    argc: c_int,
    argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    ENTRIES.with(|c| c.set(c.get() + 1));
    if argc != 1 || argv.is_null() {
        // SAFETY: `env` is live (the caller contract).
        return unsafe { crate::nif::enif::exception::enif_make_badarg(env) };
    }
    let mut n: c_int = 0;
    // SAFETY: `argv` is readable for `argc` (== 1) terms.
    if unsafe { enif_get_int(env, *argv, &mut n) } == 0 {
        // SAFETY: as above.
        return unsafe { crate::nif::enif::exception::enif_make_badarg(env) };
    }
    // SAFETY: `env` is live; a small int needs no heap.
    unsafe { enif_make_int(env, n) }
}

/// Raises `boom` **and** records a reschedule: a raise is terminal, so `cont_tail` must never run.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_raise_then_reschedule(
    env: *mut ErlNifEnv,
    _argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    ENTRIES.with(|c| c.set(c.get() + 1));
    // SAFETY: `env` is live (the caller contract).
    let boom = unsafe { crate::nif::enif::make::enif_make_atom(env, c"boom".as_ptr()) };
    // SAFETY: as above.
    let raised = unsafe { enif_raise_exception(env, boom) };
    // SAFETY: `env` is live, the name is NUL-terminated, `argv` is readable for `argc` (== 1).
    let mut next = [unsafe { enif_make_int(env, 1) }];
    // SAFETY: as above.
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
    // SAFETY: `env` is live; a small int needs no heap.
    unsafe { enif_make_int(env, 0) }
}

/// Answers the `enif_thread_type()` its continuation saw — the dirty-scheduler observable.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_thread_type(
    env: *mut ErlNifEnv,
    _argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    // SAFETY: no pointers are taken by `enif_thread_type`.
    let ty = unsafe { enif_thread_type() };
    // SAFETY: `env` is live; a small int needs no heap.
    unsafe { enif_make_int(env, ty) }
}

/// The **exact** shape of `a3_schedule.c`'s `do_work/1` → `{dirty, Arg}`: the continuation builds the
/// tuple from *its* `argv[0]`, so the value proves both that it ran and that it saw the scheduling
/// call's arguments.
///
/// # Safety
///
/// The C NIF entry-point contract: `env` live, `argv` readable for `argc` terms.
unsafe extern "C" fn cont_dirty_pair(
    env: *mut ErlNifEnv,
    argc: c_int,
    argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    ENTRIES.with(|c| c.set(c.get() + 1));
    if argc != 1 || argv.is_null() {
        // SAFETY: `env` is live (the caller contract).
        return unsafe { crate::nif::enif::exception::enif_make_badarg(env) };
    }
    // SAFETY: `argv` is readable for `argc` (== 1) terms; `env` is live.
    let words = [unsafe { crate::nif::enif::make::enif_make_atom(env, c"dirty".as_ptr()) }, unsafe {
        *argv
    }];
    // SAFETY: `env` is live and `words` is a live 2-element array for the duration of the call.
    unsafe { crate::nif::enif::make::enif_make_tuple_from_array(env, words.as_ptr(), 2) }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

/// The oracle's `sched_sum(2,3)` → `{scheduled,5}`: the continuation runs, with the caller's own
/// arguments, and its value becomes the call's result.
#[test]
fn a_continuations_value_reaches_the_caller() {
    let mut host = SchedHost::new();
    let env = NifEnv::new(&mut host);
    ENTRIES.with(|c| c.set(0));
    let argv = [n(make_small(2)), n(make_small(5))];
    // SAFETY: `env` is live, the name is NUL-terminated, `argv` is readable for 2 terms.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_sum".as_ptr(),
            0,
            Some(cont_sum),
            2,
            argv.as_ptr(),
        )
    };
    assert_eq!(
        t(term),
        make_small(7),
        "the continuation's value is the call's result (the trampoline folds the C's trap re-run in)"
    );
    assert_eq!(ENTRIES.with(Cell::get), 1, "the continuation ran exactly once");
    let (argc, argv_ptr, w0, w1) = SUM_SEEN.with(Cell::get);
    assert_eq!(argc, 2, "the continuation is entered with the scheduled argc");
    assert_eq!(
        argv_ptr,
        argv.as_ptr() as usize,
        "the outermost schedule passes the caller's argv pointer straight through — nothing is copied"
    );
    assert_eq!(
        (w0, w1),
        (argv[0].raw(), argv[1].raw()),
        "the words the continuation reads are the words the scheduling call passed in"
    );
    assert_eq!(host.yields, 1, "one yield point per trampoline iteration");
    assert!(
        env.pending_exception().is_none(),
        "a value-producing chain raises nothing"
    );
    assert!(
        host.legacy_hook_calls.is_empty(),
        "NifHost::schedule_nif is a legacy hook the entry point no longer consults"
    );
    assert!(!in_continuation(), "the trampoline is not running afterwards");
}

/// The oracle's `sched_last_ret()` == `0` and `sched_ghost_name(99)` → `{ghost,99}`: `fun_name` is
/// interned and never resolved, and a **nested** schedule answers the C's `THE_NON_VALUE`.
#[test]
fn a_nested_schedule_answers_the_non_value_and_the_outermost_yields_the_value() {
    let mut host = SchedHost::new();
    let env = NifEnv::new(&mut host);
    ENTRIES.with(|c| c.set(0));
    OUTER_SEEN.with(|c| c.set((usize::MAX, false)));
    // SAFETY: `env` is live, the name is NUL-terminated, `argc` is 0 with a null `argv`.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"no_such_nif_xyz".as_ptr(),
            0,
            Some(cont_outer),
            0,
            core::ptr::null(),
        )
    };
    assert_eq!(
        t(term),
        make_small(11),
        "the outermost call yields the value the chain ended with"
    );
    let (nested_word, inside) = OUTER_SEEN.with(Cell::get);
    assert_eq!(
        nested_word,
        Eterm::non_value().raw(),
        "a nested enif_schedule_nif answers THE_NON_VALUE (0) to its C caller, as the oracle does"
    );
    assert!(inside, "the outer continuation knows a chain is running");
    assert_eq!(
        ENTRIES.with(Cell::get),
        2,
        "both continuations ran, each exactly once"
    );
    assert_eq!(
        host.yields, 2,
        "the yield point fires once per iteration, not once per schedule call"
    );
    assert!(
        host.interned.iter().any(|n| n == "no_such_nif_xyz"),
        "fun_name is interned (measured: the name is never resolved, so a ghost name still works)"
    );
    assert!(!in_continuation(), "the trampoline is not running afterwards");
}

/// The oracle's `sched_loop(100000)` → `{done,100001}` with an unchanged `stack_size`: a
/// self-rescheduling chain completes in constant Rust stack.
///
/// The chain length is the pin: a per-reschedule recursion would need ~`N` native frames (a libtest
/// thread gets a 2 MiB stack; 100 000 frames cannot fit), so this test overflows instead of passing if
/// the trampoline ever recursed.
#[test]
fn a_self_rescheduling_chain_completes_with_a_constant_stack() {
    let mut host = SchedHost::new();
    let env = NifEnv::new(&mut host);
    CHAIN_ENTRIES.with(|c| c.set(0));
    const N: c_int = 100_000;
    let argv = [n(make_small(i64::from(N)))];
    // SAFETY: `env` is live, the name is NUL-terminated, `argv` is readable for 1 term.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_chain".as_ptr(),
            0,
            Some(cont_chain),
            1,
            argv.as_ptr(),
        )
    };
    assert_eq!(
        t(term),
        make_small(i64::from(N) + 1),
        "the chain answers the entry count the oracle reports for sched_loop(100000)"
    );
    assert_eq!(CHAIN_ENTRIES.with(Cell::get), N as usize + 1);
    assert_eq!(
        host.yields,
        N as usize + 1,
        "the host's preemption point was observed on every iteration (the oracle's fairness seam)"
    );
    assert!(
        env.pending_exception().is_none(),
        "the chain raised nothing"
    );
    assert!(!in_continuation());
}

/// The oracle's `sched_raise()` → `error:boom`: a raise inside a continuation ends the chain and is
/// terminal — a reschedule recorded by the same continuation never runs.
#[test]
fn a_raise_inside_a_continuation_ends_the_chain() {
    let mut host = SchedHost::new();
    let env = NifEnv::new(&mut host);
    ENTRIES.with(|c| c.set(0));
    TAIL_RUNS.with(|c| c.set(0));
    // SAFETY: `env` is live, the name is NUL-terminated, `argc` is 0 with a null `argv`.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_raise_then_reschedule".as_ptr(),
            0,
            Some(cont_raise_then_reschedule),
            0,
            core::ptr::null(),
        )
    };
    let boom = env
        .host()
        .intern_atom("boom")
        .expect("interns unconditionally");
    assert_eq!(
        env.pending_exception(),
        Some(boom),
        "the raise is left pending on the env (the host turns it into error:boom)"
    );
    assert_eq!(
        t(term),
        boom,
        "the outermost return word is the raised reason, exactly as enif_raise_exception answers it"
    );
    assert_eq!(
        TAIL_RUNS.with(Cell::get),
        0,
        "a raise takes precedence over a recorded reschedule: the chain is over"
    );
    assert_eq!(ENTRIES.with(Cell::get), 1, "only the raising continuation ran");
    assert!(!in_continuation());
}

/// A chain scheduled onto an env that **already** carries a pending exception never runs: the raise is
/// checked first every iteration, so an `exception_thrown` env blocks the trap exactly as it does in
/// the C (where `execute_nif`/`handle_error` sees `exception_thrown` and never re-runs the trap,
/// `erl_nif.c:3527-3541`). Nothing is fabricated either way — the reason stays the env's.
#[test]
fn a_pending_exception_blocks_the_chain_before_it_runs() {
    let mut host = SchedHost::new();
    let env = NifEnv::new(&mut host);
    ENTRIES.with(|c| c.set(0));
    let boom = env
        .host()
        .intern_atom("boom")
        .expect("interns unconditionally");
    env.set_exception(boom);
    let argv = [n(make_small(1)), n(make_small(2))];
    // SAFETY: `env` is live, the name is NUL-terminated, `argv` is readable for 2 terms.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_sum".as_ptr(),
            0,
            Some(cont_sum),
            2,
            argv.as_ptr(),
        )
    };
    assert_eq!(
        t(term),
        boom,
        "the outermost return word is the already-pending reason, not a value"
    );
    assert_eq!(
        ENTRIES.with(Cell::get),
        0,
        "the raise is checked first, so no continuation ran"
    );
    assert_eq!(host.yields, 0, "not even one yield point was reached");
    assert_eq!(
        env.pending_exception(),
        Some(boom),
        "the reason is untouched (this crate never clears the slot)"
    );
    assert!(
        !in_continuation(),
        "no chain was entered, so no nesting depth was left behind"
    );
}

/// The oracle's `sched_dirty(7)` → `{dirty,7}`, in the fixture's *own* shape: the continuation builds
/// a 2-tuple from its `argv[0]`, run under `ERL_NIF_DIRTY_JOB_CPU_BOUND`.
#[test]
fn a_dirty_flagged_continuation_produces_the_oracle_tuple() {
    let mut host = SchedHost::new();
    let env = NifEnv::new(&mut host);
    ENTRIES.with(|c| c.set(0));
    let argv = [n(make_small(7))];
    // SAFETY: `env` is live, the name is NUL-terminated, `argv` is readable for 1 term.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_dirty_pair".as_ptr(),
            ERL_DIRTY_JOB_CPU_BOUND,
            Some(cont_dirty_pair),
            1,
            argv.as_ptr(),
        )
    };
    // SAFETY: `is_tuple` only reads the header of a boxed term; the elements are on the live env heap.
    let elements: Vec<Eterm> = unsafe {
        assert!(
            beam_term::prelude::is_tuple(t(term)),
            "the continuation answered a tuple, not the C's 0"
        );
        (0..beam_term::prelude::tuple_size(t(term)))
            .map(|i| beam_term::prelude::tuple_element(t(term), i))
            .collect()
    };
    let dirty = env.host().intern_atom("dirty").expect("interns");
    assert_eq!(
        elements,
        vec![dirty, make_small(7)],
        "ORACLE `sched_dirty(7) -> {{dirty,7}}`: the dirty-flagged continuation ran with the caller's argv"
    );
    assert_eq!(ENTRIES.with(Cell::get), 1);
    assert!(env.pending_exception().is_none());
}

/// The oracle's `sched_dirty(7)` → `{dirty,7}` with the continuation's `enif_thread_type()` == `2`:
/// a dirty-flagged continuation reports the dirty scheduler type, and nothing else changes it.
#[test]
fn a_dirty_flagged_continuation_reports_a_dirty_thread_type() {
    let mut host = SchedHost::new();
    // The value `enif_thread_type` answers on this (non-scheduler, non-created) thread when no
    // dirty-flagged continuation is running — the "unchanged" baseline.
    // SAFETY: no pointers are taken.
    let unmarked = unsafe { enif_thread_type() };
    assert_eq!(unmarked, sync::ERL_NIF_THR_UNDEFINED);
    let cases = [
        (0, unmarked, "a normal schedule keeps the thread's own answer"),
        (
            ERL_DIRTY_JOB_CPU_BOUND,
            sync::ERL_NIF_THR_DIRTY_CPU_SCHEDULER,
            "ERL_NIF_DIRTY_JOB_CPU_BOUND reports the dirty CPU scheduler (2)",
        ),
        (
            ERL_DIRTY_JOB_IO_BOUND,
            sync::ERL_NIF_THR_DIRTY_IO_SCHEDULER,
            "ERL_NIF_DIRTY_JOB_IO_BOUND reports the dirty IO scheduler (3)",
        ),
        (
            ERL_DIRTY_JOB_CPU_BOUND | ERL_DIRTY_JOB_IO_BOUND,
            sync::ERL_NIF_THR_DIRTY_CPU_SCHEDULER,
            "both bits set reports the dirty CPU scheduler, deterministically",
        ),
    ];
    for (flags, expected, what) in cases {
        let env = NifEnv::new(&mut host);
        // SAFETY: `env` is live, the name is NUL-terminated, `argc` is 0 with a null `argv`.
        let term = unsafe {
            enif_schedule_nif(
                env.as_ptr(),
                c"cont_thread_type".as_ptr(),
                flags,
                Some(cont_thread_type),
                0,
                core::ptr::null(),
            )
        };
        assert_eq!(
            t(term),
            make_small(i64::from(expected)),
            "flags={flags}: {what}"
        );
    }
    // Outside any continuation the answer is the thread's own again.
    // SAFETY: no pointers are taken.
    assert_eq!(
        unsafe { enif_thread_type() },
        unmarked,
        "the dirty marker is restored when the chain ends"
    );
}

/// Every refusal keeps the C's code, in the C's order, and never runs anything.
///
/// All of them are the same wire observable — `badarg`, raised through `enif_make_badarg`
/// (`erl_nif.c:3571-3601`), which is why the *ordering* is pinned through the side effects that must
/// not happen: nothing is interned by a call refused before the intern, and no continuation runs.
#[test]
fn every_refusal_is_the_badarg_raise_in_the_cs_order() {
    let mut host = SchedHost::new();
    let env = NifEnv::new(&mut host);
    ENTRIES.with(|c| c.set(0));
    let argv = [n(make_small(1))];
    let name = c"cont_sum";
    let expected = badarg(env.host());
    // A name that must never be interned, because `argc > MAX_ARG` is checked first.
    let uninterned = c"must_not_intern";
    struct Probe {
        label: &'static str,
        name: *const c_char,
        flags: c_int,
        fptr: Option<NifContinuation>,
        argc: c_int,
        argv: *const ERL_NIF_TERM,
    }
    let probes = [
        Probe {
            label: "argc > MAX_ARG (255) is checked before anything else (erl_nif.c:3569)",
            name: uninterned.as_ptr(),
            flags: 0,
            fptr: Some(cont_sum),
            argc: MAX_ARG + 1,
            argv: argv.as_ptr(),
        },
        Probe {
            label: "a negative argc cannot be a NIF entry point's count",
            name: name.as_ptr(),
            flags: 0,
            fptr: Some(cont_sum),
            argc: -1,
            argv: argv.as_ptr(),
        },
        Probe {
            label: "a flags bit outside the two dirty-job bits (erl_nif.c:3584-3598)",
            name: name.as_ptr(),
            flags: 4,
            fptr: Some(cont_sum),
            argc: 1,
            argv: argv.as_ptr(),
        },
        Probe {
            label: "a null fun_name (the C would fault intern-ing it)",
            name: core::ptr::null(),
            flags: 0,
            fptr: Some(cont_sum),
            argc: 1,
            argv: argv.as_ptr(),
        },
        Probe {
            label: "a non-UTF-8 fun_name",
            name: c"\xff\xfe".as_ptr(),
            flags: 0,
            fptr: Some(cont_sum),
            argc: 1,
            argv: argv.as_ptr(),
        },
        Probe {
            label: "a null function pointer (the C would call through it)",
            name: name.as_ptr(),
            flags: 0,
            fptr: None,
            argc: 1,
            argv: argv.as_ptr(),
        },
        Probe {
            label: "a null argv with argc > 0 (the C would read through it)",
            name: name.as_ptr(),
            flags: 0,
            fptr: Some(cont_sum),
            argc: 1,
            argv: core::ptr::null(),
        },
    ];
    for probe in probes {
        // SAFETY: every argument is either valid or the refusal this probe is about; the ones that
        // are refused are refused before they are read.
        let term = unsafe {
            enif_schedule_nif(
                env.as_ptr(),
                probe.name,
                probe.flags,
                probe.fptr,
                probe.argc,
                probe.argv,
            )
        };
        assert_eq!(
            t(term),
            expected,
            "{} must be refused with the badarg term",
            probe.label
        );
        assert_eq!(
            env.pending_exception(),
            Some(expected),
            "{} must be raised, not merely returned",
            probe.label
        );
        // Reset the slot so the next probe's raise is its own.
        env.clear();
    }
    assert_eq!(
        ENTRIES.with(Cell::get),
        0,
        "no refusal may run a continuation"
    );
    assert!(
        !host.interned.iter().any(|n| n == "must_not_intern"),
        "argc > MAX_ARG is checked before fun_name is interned"
    );
    assert!(
        host.legacy_hook_calls.is_empty(),
        "no refusal consults the legacy schedule_nif hook"
    );
    // A null env is the entry point's last refusal: no host means nothing can run, and the answer is
    // the crate's env-less `badarg` (`NIL` — there is no env to raise on).
    // SAFETY: `argc` is 0 with a null `argv`, so nothing is read.
    let term = unsafe {
        enif_schedule_nif(
            core::ptr::null_mut(),
            name.as_ptr(),
            0,
            Some(cont_sum),
            0,
            core::ptr::null(),
        )
    };
    assert_eq!(
        t(term),
        NIL,
        "a null env answers the env-less badarg marker"
    );
    assert_eq!(ENTRIES.with(Cell::get), 0);
}

/// `argc == 0` with a null `argv` is the legal "no arguments" form — only a *non-zero* count with a
/// null vector is refused.
#[test]
fn a_null_argv_is_legal_when_there_are_no_arguments() {
    let mut host = SchedHost::new();
    let env = NifEnv::new(&mut host);
    // SAFETY: `argc` is 0 with a null `argv`, so neither is read.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            c"cont_thread_type".as_ptr(),
            0,
            Some(cont_thread_type),
            0,
            core::ptr::null(),
        )
    };
    // The continuation answers `enif_thread_type()`, which is the thread's own (unmarked) type.
    assert_eq!(
        t(term),
        make_small(i64::from(sync::ERL_NIF_THR_UNDEFINED)),
        "the zero-argument continuation ran"
    );
    assert!(
        env.pending_exception().is_none(),
        "a legal zero-argument schedule raises nothing"
    );
}
