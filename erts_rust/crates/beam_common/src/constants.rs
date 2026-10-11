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

//! Canonical ERTS constants shared by the Rust workspace.
//!
//! This module is the designated home for ERTS constants that are genuinely shared by
//! **two or more** workspace crates. `beam_common` is the lowest common ancestor of every
//! crate in `erts_rust`, so it owns the single canonical definition of each such value;
//! dependents re-export or reference it instead of transcribing their own copy. A constant
//! belongs here only when it is genuinely shared — values used by a single crate stay in
//! that crate.

/// The ERTS reduction quantum: the number of reductions a process (or port) may execute
/// before the scheduler preempts it and swaps in another runnable entity — the context-switch
/// threshold, in other words.
///
/// Ground truth is the C runtime, `erts/emulator/beam/erl_vm.h:47`:
///
/// ```c
/// #define CONTEXT_REDS 4000            /* Swap process out after this number */
/// ```
///
/// This is a **frozen ERTS interface constant**: `erlang:system_info(context_reductions)`
/// reports exactly `4000`, so the value is observable and must not drift.
///
/// This is the single canonical definition within the Rust workspace. Dependents reference or
/// re-derive from it instead of transcribing the value; the call sites (path in parentheses):
///
/// * `beam_process::scheduler` (`crates/beam_process/src/scheduler.rs`) re-derives it as the
///   per-process slice budget `DEFAULT_REDUCTIONS_PER_SLICE = CONTEXT_REDS as u32`.
/// * `beam_rt::info::schedulers` (`crates/beam_rt/src/info/schedulers.rs`) reports
///   `erlang:system_info(context_reductions)` as `CONTEXT_REDS`.
/// * `beam_io::port_task` (`crates/beam_io/src/port_task.rs`) re-exports it and derives its
///   port-task reduction budgets from it.
pub const CONTEXT_REDS: usize = 4000;

#[cfg(test)]
mod tests {
    use super::*;

    // Pinned to the C runtime's frozen interface constant:
    // `erts/emulator/beam/erl_vm.h:47` → `#define CONTEXT_REDS 4000`.
    #[test]
    fn context_reds_is_the_frozen_erlang_value() {
        assert_eq!(CONTEXT_REDS, 4000);
    }
}
