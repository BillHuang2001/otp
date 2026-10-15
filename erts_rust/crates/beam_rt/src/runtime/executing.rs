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

//! The runtime's own **root execution frame** — which *root action* is driving the process
//! this runtime runs itself, and therefore what `erlang:process_info/2`'s execution-position
//! items answer for the **caller** (`process_info(self(), current_function | initial_call |
//! current_location | catchlevel)`).
//!
//! # Why a separate record is needed
//!
//! Every *other* process's position is read out of the engine the runtime holds for it
//! ([`crate::runner::RunnerState::position_engine_at`]). The runtime's **own** process has no
//! such engine: a `-eval`/`-s`/`-run` action is driven by a `beam_engine::Machine` created
//! *inside* [`crate::context::RuntimeContext::run_export`] (for `-s`/`-run`) or by the
//! `-eval` evaluator ([`crate::eval`]). A query the **compiled code** of that run dispatches
//! reads that machine's live position through the in-machine BIF door
//! ([`crate::context::position`]); a query driven **outside** it — the `-eval` evaluator's
//! own tree-walk, or a library caller of
//! [`ExecutionContext::call_bif`](beam_engine::ExecutionContext::call_bif) — has no machine
//! to read. This frame is the *sourceable* part of the caller's position in that second
//! case: the **shape of the root action** that established the process, from which the
//! oracle's own values for those items follow (see
//! [`crate::native::memory::RuntimeContext::position_value`] for the oracle evidence).
//!
//! # Invariants
//!
//! * The frame is written **only** by the root-action entries — [`Runtime::eval`]
//!   ([`RootAction::Eval`]) and [`Runtime::run_named`] ([`RootAction::Module`]) — around the
//!   whole run, and it is **restored on every exit path**, so a stale frame can never be read
//!   after the action returned (a nested action restores the frame it replaced).
//! * It names the runtime's **own** pid ([`Runtime::self_pid`]) — the process `erlang:self/0`
//!   reports — so a probe of any *other* pid falls through to the engine path unchanged.
//! * A caller with no frame **and** no machine (a library driving
//!   [`ExecutionContext::call_bif`](beam_engine::ExecutionContext::call_bif) directly) has no
//!   source for a position, and the items stay the honest typed capability gap there.

use beam_process::ProcessId;

use crate::runtime::Runtime;

/// Which **root action** is driving the runtime's own process.
///
/// The two variants carry exactly the information the `process_info/2` execution-position
/// items need, and nothing more: whether a compiled module frame is on the stack at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootAction {
    /// `-eval`: the runtime's `erl_eval`-style evaluator ([`Runtime::eval`]) is the driver.
    ///
    /// There is no compiled module frame in this shape unless the evaluated expression
    /// calls one, so the driver's own frame (`erl_eval:do_apply/7` in the oracle) is the
    /// innermost position while no module is entered.
    Eval,
    /// `-s`/`-run`: a **module export** entered synchronously ([`Runtime::run_named`]).
    ///
    /// The stack always holds a compiled module frame here, whose *innermost* function is
    /// inside the machine (unreachable from the position reader — see the module docs), so
    /// only the items that root action itself sources are answered.
    Module,
}

/// The root action currently driving the runtime's own process, with the pid it belongs to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExecutingFrame {
    /// The pid the action drives — always the runtime's own [`Runtime::self_pid`].
    pub(crate) pid: ProcessId,
    /// Which root action is driving it.
    pub(crate) action: RootAction,
}

impl Runtime {
    /// Records `action` as the root action driving the runtime's own process and returns the
    /// frame it replaced, so a **nested** root action can restore its caller's frame
    /// ([`Runtime::leave_root_action`]).
    ///
    /// The only callers are the root-action entries themselves — [`Runtime::eval`] and
    /// [`Runtime::run_named`] — which bracket their whole run with this and
    /// [`Runtime::leave_root_action`], so the frame is never observable outside a root run.
    pub(crate) fn enter_root_action(&mut self, action: RootAction) -> Option<ExecutingFrame> {
        let pid = self.self_pid;
        self.executing.replace(ExecutingFrame { pid, action })
    }

    /// Restores the frame [`Runtime::enter_root_action`] replaced — the saved frame of a
    /// nesting root action, or `None` (no frame) when there was none.
    pub(crate) fn leave_root_action(&mut self, previous: Option<ExecutingFrame>) {
        self.executing = previous;
    }

    /// The root action driving `pid`, if this runtime is driving **that** process itself.
    ///
    /// `None` for every process the runtime did not establish through a root action — a
    /// scheduled process, a foreign pid, and the runtime's own process when it was not
    /// entered through [`Runtime::eval`]/[`Runtime::run_named`].
    pub(crate) fn root_action_of(&self, pid: ProcessId) -> Option<RootAction> {
        self.executing
            .filter(|frame| frame.pid == pid)
            .map(|frame| frame.action)
    }
}
