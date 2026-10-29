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

//! The tree-walker's **application-nesting budget**.
//!
//! # Why a budget is needed
//!
//! Applying one of this evaluator's closures is a chain of native Rust calls
//! ([`super::Evaluator::apply_closure`] → the clause body → the recursive call → `apply_closure`
//! again), so a self-recursive named fun nests one Rust frame chain per level — and a Rust stack
//! overflow is a process **abort** (`SIGABRT`), never an Erlang result and never a typed error.
//!
//! Neither side of that cost is a constant:
//!
//! * **One level's cost depends on the shape of the clause body.** Each level is the ten frames
//!   `apply_closure`/`walk_closure`/`exprs`/`expr`/`expr_inner`/`list`/`local_call`/`apply_value`
//!   and their locals, and a cons-building body adds the whole list/operand-evaluation path.
//!   Measured through the headroom probe ([`native_stack::headroom_bytes`]) on a 2 MiB libtest
//!   thread, dev profile (`opt-level = 1`): `G(N) -> G(N - 1)` ≈1.4 KiB per level,
//!   `G(N, Acc) -> G(N - 1, [N | Acc])` ≈3.0 KiB, `G(N) -> [N | G(N - 1)]` ≈3.9 KiB.
//! * **The thread's stack depends on the entry point.** libtest hands every test a **2 MiB**
//!   stack — the smallest of the three this crate runs under — `main` has 8 MiB, and the binary's
//!   `beam_rt-driver` thread (`src/main.rs`) 64 MiB.
//!
//! A fixed counter therefore cannot be safe everywhere: the same 512-application budget costs
//! ≈0.7 MiB of the tail shape and ≈2 MiB of the cons shape, and the cons shape aborts a 2 MiB
//! test thread at nesting 394 — before any counter of 512 could fire.
//!
//! # Two bounds: the physical one decides *safety*, the counter decides *policy*
//!
//! [`Slot::enter`] is called once per walker closure application and answers which route may run
//! the body:
//!
//! * [`Route::Walk`] — the body runs in the tree walker, exactly as every ordinary `-eval`
//!   closure always has. Granted while **both** bounds allow it: this thread's remaining native
//!   stack is at least [`WALK_STACK_RESERVE`] **and** the nesting is within [`WALK_APPLY_LIMIT`].
//! * [`Route::Compiled`] — the walker may not (or should no longer) nest another level, so the
//!   body runs as **compiled code** (`crate::eval::compile`): the closure's recorded source text
//!   becomes a real module, and its recursion stays on the BEAM process stack, where the engine
//!   grows it on the heap. The value is the oracle's.
//! * [`Route::Exhausted`] — [`COMPILED_APPLICATION_LIMIT`] compiled applications are already
//!   nested, which is the shape where the recursion alternates compiled-module → `apply_fun` →
//!   walker (a fun value passed to itself, say). Nothing can turn that into pure BEAM recursion,
//!   so the application is refused with the typed `unsupported` gap ([`exhausted`]) instead of
//!   growing the Rust stack without bound.
//!
//! The physical bound is the primary one: it is the only thing that can be *correct* on every
//! thread, because it is measured rather than assumed. The counter remains necessary, because
//! the reserve is a **depth** bound rather than a budget — on the 64 MiB driver thread 512 levels
//! of the most expensive shape cost ≈2 MiB, far above the reserve, so the counter is what bounds
//! the walker's recursion there and what keeps ordinary `-eval` routing independent of a larger
//! stack.
//!
//! # The reserve, and how it was measured
//!
//! [`WALK_STACK_RESERVE`] must cover, from the moment the walk stops: the rest of the level the
//! decision was taken in (≤4 KiB, measured), the [`Route::Compiled`] path
//! ([`compile::entry_for`](super::compile::entry_for)'s in-process `compile:file/2` run plus the
//! nested machine for the compiled body), and up to [`COMPILED_APPLICATION_LIMIT`] nested
//! compiled applications.
//!
//! Measured: forcing the switch at ≈164 KiB of remaining stack (a 1.9 MiB reserve on a 2 MiB
//! libtest thread) still answers every deep recursion — the non-tail cons build, the
//! accumulator, and the worst case, the 16-deep alternating shape — so the compiled route needs
//! **less than ≈160 KiB**. The 1 MiB reserve therefore leaves ≈6× margin, and on a 2 MiB thread
//! it keeps ≈1 MiB for the walk: 259 levels of the most expensive shape and 327 of the
//! accumulator shape before the switch, while a 100-level recursion (≈140 KiB) stays walked by a
//! wide margin and the differential corpus never changes route.
//!
//! On a thread whose remaining stack is below the reserve, an application a bigger stack would
//! have walked is taken by the compiled route — and if that closure cannot be compiled (no
//! recorded source text, a context inside a scheduler slice) the application is the **typed**
//! [`compile_declined`] gap rather than an abort. That narrowing is deliberate: a typed refusal
//! is a reportable limitation, an abort is not.
//!
//! # Why the counters are thread-local
//!
//! They must outlive one [`Evaluator`](super::Evaluator): `apply_eval_closure` builds a **fresh**
//! evaluator (with its own `depth: 0`) whenever a loaded module applies this evaluator's closure,
//! so a per-evaluator field would reset the budget at exactly the mutual-recursion seam the
//! [`COMPILED_APPLICATION_LIMIT`] exists for. The `-eval` route is single-threaded per driver, so
//! a thread-local is the right scope: one thread's nesting is one Rust stack. The headroom is
//! thread-local for the same reason (and [`native_stack`] caches one probe per thread).

use core::cell::Cell;

use beam_common::BeamError;

use super::native_stack;

/// How many walker closure applications may be nested on the Rust stack.
///
/// Past this the body runs as compiled code (see the module docs). This is the *policy* bound:
/// the physical one ([`WALK_STACK_RESERVE`]) can fire first on a thread with a small stack.
pub(crate) const WALK_APPLY_LIMIT: usize = 512;

/// How many compiled-closure applications may nest *inside* one another once the walker may no
/// longer walk.
///
/// Each of these costs a whole cycle — compiled module → `apply_fun` → fresh evaluator → walker
/// → compiled module — so the budget is what keeps that alternation from being an unbounded Rust
/// recursion of its own. Exceeding it answers the typed [`exhausted`] gap.
pub(crate) const COMPILED_APPLICATION_LIMIT: usize = 16;

/// Native stack bytes the walker must still have below it before it may walk one more level.
///
/// See the module docs for what it covers and how it was measured: the rest of the current level
/// (≤4 KiB), the compile route the decision hands the body to, and the compiled applications that
/// may nest inside it — measured together at **<≈160 KiB**, so 1 MiB leaves ≈6× margin.
pub(crate) const WALK_STACK_RESERVE: usize = 1024 * 1024;

thread_local! {
    /// Walker closure applications currently on this thread's stack.
    static WALK_NESTING: Cell<usize> = const { Cell::new(0) };
    /// Compiled-closure applications currently nested inside the walker's bound.
    static COMPILED_NESTING: Cell<usize> = const { Cell::new(0) };
}

/// Whether this thread's remaining native stack is above [`WALK_STACK_RESERVE`].
///
/// `None` from the probe (a platform with no stack-bounds answer) keeps the counter-only
/// behaviour, i.e. the walk is allowed — the honest fallback [`native_stack`] documents.
fn walk_has_headroom() -> bool {
    native_stack::headroom_bytes().is_none_or(|headroom| headroom >= WALK_STACK_RESERVE)
}

/// Which route a closure application must take (see the module docs).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Route {
    /// The body runs in the tree walker, on the Rust stack.
    Walk,
    /// The body runs as compiled code, on the BEAM process stack.
    Compiled,
    /// No route is left: the application is the typed [`exhausted`] gap.
    Exhausted,
}

/// One closure application's budget slot — the counters are restored when it drops.
///
/// The guard covers the whole application (body, compile, module call), so the counters describe
/// the Rust stack even when the application returns a raise: every exit path goes through
/// [`Slot::drop`]. A slot whose route is [`Route::Compiled`] because the *headroom* ran out is
/// accounted exactly like one that reached [`WALK_APPLY_LIMIT`]: the application is still a
/// walker-nesting level (the walker's frames are on the stack either way), and the compiled budget
/// is what bounds how many of them may nest.
#[derive(Debug)]
pub(crate) struct Slot {
    /// Whether this slot took one of the [`COMPILED_APPLICATION_LIMIT`] compiled slots.
    compiled: bool,
}

impl Slot {
    /// Takes one application's slot and answers the route it may use.
    pub(crate) fn enter() -> (Slot, Route) {
        let nesting = WALK_NESTING.with(|counter| {
            let nesting = counter.get() + 1;
            counter.set(nesting);
            nesting
        });
        if nesting <= WALK_APPLY_LIMIT && walk_has_headroom() {
            return (Slot { compiled: false }, Route::Walk);
        }
        let compiled = COMPILED_NESTING.with(|counter| {
            let taken = counter.get();
            if taken < COMPILED_APPLICATION_LIMIT {
                counter.set(taken + 1);
                true
            } else {
                false
            }
        });
        if compiled {
            (Slot { compiled: true }, Route::Compiled)
        } else {
            (Slot { compiled: false }, Route::Exhausted)
        }
    }

    /// How many walker applications are nested on this thread right now (test instrumentation).
    #[cfg(test)]
    pub(crate) fn walk_nesting() -> usize {
        WALK_NESTING.with(|counter| counter.get())
    }

    /// How many compiled applications are nested on this thread right now (test instrumentation).
    #[cfg(test)]
    pub(crate) fn compiled_nesting() -> usize {
        COMPILED_NESTING.with(|counter| counter.get())
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        WALK_NESTING.with(|counter| counter.set(counter.get().saturating_sub(1)));
        if self.compiled {
            COMPILED_NESTING.with(|counter| counter.set(counter.get().saturating_sub(1)));
        }
    }
}

/// The typed refusal of an application the walker cannot nest any further.
///
/// Its reason carries the `unsupported` prefix, so the `-eval` driver turns it into the route's
/// uncatchable capability gap ([`super::Evaluator::flow_from_error`]) and the CLI reports
/// `beam_rt: unsupported: <reason>` with exit code
/// [`EXIT_UNSUPPORTED`](crate::cli::EXIT_UNSUPPORTED) — a typed refusal, never a stack overflow.
pub(crate) fn exhausted() -> BeamError {
    crate::error::unsupported(&format!(
        "eval fun application: {COMPILED_APPLICATION_LIMIT} compiled closures are already nested \
         inside the tree-walking evaluator's {WALK_APPLY_LIMIT}-application recursion bound"
    ))
}

/// The typed refusal of an application past the bound whose body **cannot be compiled**, `why`
/// naming the reason (no recorded source text, a compile that declined, a context that may not
/// compile, …).
///
/// Same classification as [`exhausted`]: an `unsupported` capability gap, so the caller never
/// mistakes it for a value or for a wrong result.
pub(crate) fn compile_declined(why: &str) -> BeamError {
    crate::error::unsupported(&format!(
        "eval fun application: the tree-walking evaluator's {WALK_APPLY_LIMIT}-application \
         recursion bound was reached and the closure cannot be compiled ({why})"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three routes in order: walker up to the bound, compiled past it, then the refusal.
    #[test]
    fn the_routes_are_offered_in_order_and_restored_on_drop() {
        let mut slots = Vec::with_capacity(WALK_APPLY_LIMIT + COMPILED_APPLICATION_LIMIT + 1);
        for _ in 0..WALK_APPLY_LIMIT {
            let (slot, route) = Slot::enter();
            assert_eq!(route, Route::Walk);
            slots.push(slot);
        }
        assert_eq!(Slot::walk_nesting(), WALK_APPLY_LIMIT);
        assert_eq!(Slot::compiled_nesting(), 0);

        for expected in 1..=COMPILED_APPLICATION_LIMIT {
            let (slot, route) = Slot::enter();
            assert_eq!(route, Route::Compiled, "compiled slot {expected}");
            assert_eq!(Slot::compiled_nesting(), expected);
            slots.push(slot);
        }

        let (refused, route) = Slot::enter();
        assert_eq!(route, Route::Exhausted);
        assert_eq!(
            Slot::compiled_nesting(),
            COMPILED_APPLICATION_LIMIT,
            "a refused application takes no compiled slot"
        );
        drop(refused);

        // Dropping every slot restores both counters, so the next application walks again.
        drop(slots);
        assert_eq!(Slot::walk_nesting(), 0);
        assert_eq!(Slot::compiled_nesting(), 0);
        let (slot, route) = Slot::enter();
        assert_eq!(route, Route::Walk);
        drop(slot);
        assert_eq!(Slot::walk_nesting(), 0);
    }

    /// A compiled slot's drop gives its compiled budget back while the walker stays nested.
    #[test]
    fn a_compiled_slot_returns_its_budget_while_the_walker_stays_nested() {
        let mut walker = Vec::with_capacity(WALK_APPLY_LIMIT);
        for _ in 0..WALK_APPLY_LIMIT {
            walker.push(Slot::enter().0);
        }
        for _ in 0..COMPILED_APPLICATION_LIMIT {
            let (slot, route) = Slot::enter();
            assert_eq!(route, Route::Compiled);
            drop(slot);
            assert_eq!(Slot::compiled_nesting(), 0);
            assert_eq!(Slot::walk_nesting(), WALK_APPLY_LIMIT);
        }
        drop(walker);
        assert_eq!(Slot::walk_nesting(), 0);
    }

    /// Both refusals are the typed `unsupported` capability gap and name the bound.
    #[test]
    fn the_refusals_are_typed_capability_gaps() {
        for refusal in [
            exhausted(),
            compile_declined("the closure has no recorded source text"),
        ] {
            let reason = crate::error::unsupported_reason(&refusal)
                .unwrap_or_else(|| panic!("{refusal} is not classified as unsupported"));
            assert!(
                reason.contains(&WALK_APPLY_LIMIT.to_string()),
                "the refusal {reason:?} does not name the bound"
            );
        }
    }

    /// **The physical bound is real, and only the measurement may be trusted**: the gate answers on
    /// this thread's *measured* headroom, never on a requested thread size — glibc serves a small
    /// `thread::Builder::stack_size` request from its freed-stack cache, so the request is not the
    /// stack. Two directions, both on stacks the suite really has:
    ///
    /// * With room to spare (the ≈2 MiB libtest thread) the first application walks.
    /// * From a stack grown down **below** [`WALK_STACK_RESERVE`] the very next application takes
    ///   the compiled route with the walker counter at `1` — a headroom-driven slot is still a
    ///   walker-nesting slot, far below [`WALK_APPLY_LIMIT`] — and both counters are restored when
    ///   it drops.
    #[test]
    fn the_headroom_gate_decides_the_route_before_the_counter_does() {
        let headroom = native_stack::headroom_bytes().expect("a supported host answers");
        if headroom >= WALK_STACK_RESERVE {
            let (slot, route) = Slot::enter();
            assert_eq!(
                route,
                Route::Walk,
                "{headroom} B of headroom is above the {WALK_STACK_RESERVE} B reserve"
            );
            assert_eq!(Slot::walk_nesting(), 1);
            assert_eq!(Slot::compiled_nesting(), 0);
            drop(slot);
            assert_eq!(Slot::walk_nesting(), 0, "the slot's drop restores its nesting");
            assert_eq!(Slot::compiled_nesting(), 0);
        }
        assert_eq!(
            enter_below_reserve(headroom, 0),
            Some((Route::Compiled, 1, 1)),
            "an application on less than {WALK_STACK_RESERVE} B of stack must not be walked"
        );
        assert_eq!(Slot::walk_nesting(), 0, "the slot's drop restores its nesting");
        assert_eq!(Slot::compiled_nesting(), 0);
    }

    /// Grows this thread's stack down by one black-boxed `DESCENT_FRAME`-byte frame per level until
    /// the **measured** headroom is below [`WALK_STACK_RESERVE`], then takes one slot there and
    /// answers `(route, walk nesting, compiled nesting)`.
    ///
    /// Each level keeps its buffer live across the recursive call, so the descent consumes at least
    /// `DESCENT_FRAME` bytes per level and leaves the reserve untouched below it; the level cap is
    /// that descent's arithmetic worst case plus two, so reaching it is a bug, never a big thread.
    fn enter_below_reserve(start_headroom: usize, level: usize) -> Option<(Route, usize, usize)> {
        const DESCENT_FRAME: usize = 32 * 1024;
        let cap = start_headroom / DESCENT_FRAME + 2;
        assert!(level <= cap, "the descent never reached the reserve in {cap} levels");

        let mut buffer = [0u8; DESCENT_FRAME];
        buffer[level % DESCENT_FRAME] = 1;
        let keep = std::hint::black_box(&buffer);
        let below = native_stack::headroom_bytes().is_some_and(|h| h < WALK_STACK_RESERVE);
        let observed = if below {
            let (slot, route) = Slot::enter();
            let observed = (route, Slot::walk_nesting(), Slot::compiled_nesting());
            drop(slot);
            Some(observed)
        } else {
            enter_below_reserve(start_headroom, level + 1)
        };
        assert_eq!(keep[level % DESCENT_FRAME], 1, "each level's frame stays live");
        observed
    }
}
