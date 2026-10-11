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

//! The match-spec arithmetic [`ArithHook`] this crate installs over `beam_ets`.
//!
//! `beam_ets` evaluates a match-spec guard/body arithmetic operator through a
//! **thread-local** seam (`beam_ets::select::expr::set_arith_hook`); without an
//! installed hook the built-in `DefaultArithHook` answers, and it is checked
//! small-integer/float only — a bignum operand or an integer result outside the
//! 60-bit small range is the typed `unsupported_ets`. This module installs the
//! runtime's own hook, backed by [`crate::arith`]'s **arbitrary-precision**
//! operators, so those cases produce the oracle's value instead.
//!
//! # Install point (and why it is scoped)
//!
//! A BIF does not own the runtime thread, and the seam has no global "install once"
//! entry, so the hook is installed **per match-spec evaluation** with
//! [`PrecisionArith::install`]. The guard is created at the top of every `ets:*`
//! BIF that evaluates a match spec (`ets:select/1,2,3`, `ets:select_reverse/1,2,3`,
//! `ets:select_count/2`, `ets:select_delete/2`, `ets:select_replace/2`,
//! `ets:match_spec_run/2`, `ets:match_spec_run_r/3`,
//! `ets:internal_select_delete/2`) and of `erlang:match_spec_test/3`, and it
//! restores the seam's default when the outermost one drops — so the hook is live
//! only while a spec is being evaluated and never leaks into unrelated work. The
//! `ets:select`/`match` continuation resumes (`/1`) run through the same guard, so a
//! resumed traversal keeps the arithmetic.
//!
//! `beam_ets`' `set_arith_hook` seam exposes no getter, so the previous hook cannot
//! be saved; this crate is the only installer on the thread, and the restored value
//! is therefore the seam's own default (`None` ⇒ `DefaultArithHook`). Nested guards
//! are depth-counted, so an inner scope never tears an outer one down.
//!
//! # Honesty
//!
//! The hook never fabricates a result: it delegates to the registered `erlang`
//! operators, whose failures (`badarith`/`system_limit`) are mapped to the `badarg`
//! the match-spec executor treats as the oracle's own raise (`crate::arith::
//! match_spec_arith`), the same class `DefaultArithHook` uses.

use std::cell::Cell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::Result;
use beam_ets::select::expr::{set_arith_hook, ArithHook, ArithOp};
use beam_term::{AtomTable, Eterm};

use crate::arith::match_spec_arith;

/// The arithmetic hook: every match-spec operator is computed by the registered
/// `erlang` operator implementations (arbitrary precision), so a bignum operand or
/// an out-of-range result is the oracle's own value rather than a typed gap.
#[derive(Debug, Clone, Copy)]
struct PrecisionArithHook;

impl ArithHook for PrecisionArithHook {
    fn eval(
        &self,
        op: ArithOp,
        args: &[Eterm],
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Eterm> {
        match_spec_arith(op, args, atoms, heap)
    }
}

thread_local! {
    /// How many [`PrecisionArith`] guards are live on this thread (nested installs
    /// share one hook and only the outermost drop restores the default).
    static INSTALL_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// A scoped installation of [`PrecisionArithHook`] on the current thread (see the
/// module docs for the install point). The hook is live for the guard's lifetime.
#[derive(Debug)]
pub(crate) struct PrecisionArith;

impl PrecisionArith {
    /// Installs the arbitrary-precision [`ArithHook`] on the current thread for the
    /// returned guard's lifetime, restoring the seam's default when the outermost
    /// guard drops.
    pub(crate) fn install() -> PrecisionArith {
        INSTALL_DEPTH.with(|depth| {
            if depth.get() == 0 {
                set_arith_hook(Some(Rc::new(PrecisionArithHook)));
            }
            depth.set(depth.get() + 1);
        });
        PrecisionArith
    }
}

impl Drop for PrecisionArith {
    fn drop(&mut self) {
        INSTALL_DEPTH.with(|depth| {
            let remaining = depth.get().saturating_sub(1);
            depth.set(remaining);
            if remaining == 0 {
                set_arith_hook(None);
            }
        });
    }
}
