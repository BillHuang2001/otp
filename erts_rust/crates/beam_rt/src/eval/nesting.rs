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
//! again), so a self-recursive named fun nests one Rust frame chain per level:
//! `F = fun G(0) -> done; G(N) -> G(N - 1) end, F(50000).` measures ≈1.4 KiB of stack per level in
//! a release build and ≈2.1 KiB in a dev build, so the binary's 64 MiB driver stack (the named
//! thread `src/main.rs` starts the driver on) overflows just below 48 000 (release) / 32 000
//! (dev) levels — and a Rust stack overflow is a process **abort** (`SIGABRT`), not an Erlang
//! result and not a typed error.
//!
//! # The bound and the three routes
//!
//! [`Slot::enter`] is called once per walker closure application and answers which route may run
//! the body:
//!
//! * [`Route::Walk`] — the nesting is within [`WALK_APPLY_LIMIT`], so the body runs in the tree
//!   walker, exactly as every `-eval` closure always has.
//! * [`Route::Compiled`] — the nesting is past the limit, so the body runs as **compiled code**
//!   (`crate::eval::compile`): the closure's recorded source text becomes a real module, and its
//!   recursion stays on the BEAM process stack, where the engine grows it on the heap. The value
//!   is the oracle's.
//! * [`Route::Exhausted`] — [`COMPILED_APPLICATION_LIMIT`] compiled applications are already
//!   nested inside the bound, which is the shape where the recursion alternates
//!   compiled-module → `apply_fun` → walker (a fun value passed to itself, say). Nothing can turn
//!   that into pure BEAM recursion, so the application is refused with the typed
//!   `unsupported` gap ([`exhausted`]) instead of growing the Rust stack without bound.
//!
//! # Why the counters are thread-local
//!
//! They must outlive one [`Evaluator`](super::Evaluator): `apply_eval_closure` builds a **fresh**
//! evaluator (with its own `depth: 0`) whenever a loaded module applies this evaluator's closure,
//! so a per-evaluator field would reset the budget at exactly the mutual-recursion seam the
//! [`COMPILED_APPLICATION_LIMIT`] exists for. The `-eval` route is single-threaded per driver, so
//! a thread-local is the right scope: one thread's nesting is one Rust stack.
//!
//! # The size of the bound
//!
//! [`WALK_APPLY_LIMIT`] is chosen from the measurements above: 512 levels cost ≈1 MiB of stack
//! even in an unoptimised build, which leaves the rest of a test thread's 8 MiB and of the
//! binary's 64 MiB untouched — and the compiled applications nested past it add a bounded
//! few hundred KiB. The differential corpus, the crate's suites and ordinary `-eval`
//! expressions stay far inside it, so nothing but a genuinely deep recursion changes route.

use core::cell::Cell;

use beam_common::BeamError;

/// How many walker closure applications may be nested on the Rust stack.
///
/// Past this the body runs as compiled code (see the module docs).
pub(crate) const WALK_APPLY_LIMIT: usize = 512;

/// How many compiled-closure applications may nest *inside* one another once the walker is at
/// [`WALK_APPLY_LIMIT`].
///
/// Each of these costs a whole cycle — compiled module → `apply_fun` → fresh evaluator → walker
/// → compiled module — so the budget is what keeps that alternation from being an unbounded Rust
/// recursion of its own. Exceeding it answers the typed [`exhausted`] gap.
pub(crate) const COMPILED_APPLICATION_LIMIT: usize = 16;

thread_local! {
    /// Walker closure applications currently on this thread's stack.
    static WALK_NESTING: Cell<usize> = const { Cell::new(0) };
    /// Compiled-closure applications currently nested inside the walker's bound.
    static COMPILED_NESTING: Cell<usize> = const { Cell::new(0) };
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
/// [`Slot::drop`].
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
        if nesting <= WALK_APPLY_LIMIT {
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
}
