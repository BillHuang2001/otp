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

//! The `schedulers` family of [`super::system_info_value`]: processor and scheduler keys.
//!
//! This runtime is **single-threaded** (see `beam_rt/CONTEXT.md`), so it reports a
//! self-consistent scheduler model derived from its own configuration: the processor
//! counts come from [`std::thread::available_parallelism`], an explicit `+S N` wins for
//! the normal scheduler count, and the dirty schedulers default to `0` (this runtime has
//! none) unless `+SDcpu`/`+SDio` configured some. The bind keys are the honest answer
//! for a runtime that never binds a scheduler to a processor.
//!
//! | key | shape |
//! |---|---|
//! | `logical_processors` / `_available` / `_online` | int (`available_parallelism`) |
//! | `schedulers` | int (`+S N` or the processor count) |
//! | `schedulers_online` | int (all schedulers are online here) |
//! | `scheduler_id` | int (`1`) |
//! | `dirty_cpu_schedulers` / `_online` | int (`+SDcpu` or `0`) |
//! | `dirty_io_schedulers` | int (`+SDio` or `0`) |
//! | `scheduler_bind_type` | atom, `unbound` |
//! | `scheduler_bindings` | `{unbound, unbound, …}`, one per scheduler |
//! | `multi_scheduling` | atom, `enabled`/`disabled` |
//! | `multi_scheduling_blockers` / `normal_multi_scheduling_blockers` | `[]` |
//! | `thread_pool_size` | int (`1`) |
//! | `context_reductions` | int (`4000`, the `CONTEXT_REDS` interface constant) |

use beam_common::Result;

use super::{logical_processors, schedulers_count, InfoValue};
use crate::runtime::Runtime;

/// `CONTEXT_REDS` (`erts/emulator/beam/erl_vm.h:47`: *"Swap process out after this
/// number"*) — the emulator's reduction quantum.
///
/// `erlang:system_info(context_reductions)` is `make_small(CONTEXT_REDS)`
/// (`erl_bif_info.c:3363-3364`), i.e. a **compile-time constant of the ERTS interface**,
/// like `atom_limit`/`ets_limit` — not a measured property of a particular run. The
/// oracle reports `4000` for every invocation (verified against OTP 29.1.1 / erts-17.1),
/// so the number is reported as the interface constant it is.
///
/// The runtime does **honour** a per-slice reduction budget of the same size: the runner
/// pre-empts (yields) a CPU-bound process once
/// `beam_process::DEFAULT_REDUCTIONS_PER_SLICE` (4000, charged at the machine's
/// heap-admission points) is spent, so this constant is also the runtime's own slice budget.
const CONTEXT_REDS: i64 = 4000;

/// The `system_info/1` value of a `schedulers`-family key, or `None` when `key` is not
/// ours.
pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>> {
    match key {
        "logical_processors" | "logical_processors_available" | "logical_processors_online" => {
            Some(Ok(InfoValue::Int(logical_processors())))
        }
        "schedulers" => Some(Ok(InfoValue::Int(schedulers_count(rt)))),
        // Every scheduler this runtime has is online (there is no online/offline model
        // yet — the oracle can report fewer online than configured under load).
        "schedulers_online" => Some(Ok(InfoValue::Int(schedulers_count(rt)))),
        // The calling scheduler's id. This runtime's one scheduler is id `1`
        // (the oracle's ids are 1-based).
        "scheduler_id" => Some(Ok(InfoValue::Int(1))),
        "dirty_cpu_schedulers" | "dirty_cpu_schedulers_online" => {
            Some(Ok(InfoValue::Int(dirty_cpu_schedulers(rt))))
        }
        "dirty_io_schedulers" => Some(Ok(InfoValue::Int(dirty_io_schedulers(rt)))),
        "scheduler_bind_type" => Some(Ok(InfoValue::Atom("unbound"))),
        "scheduler_bindings" => Some(Ok(scheduler_bindings(rt))),
        "multi_scheduling" => Some(Ok(InfoValue::Atom(if schedulers_count(rt) > 1 {
            "enabled"
        } else {
            "disabled"
        }))),
        "multi_scheduling_blockers" | "normal_multi_scheduling_blockers" => {
            Some(Ok(InfoValue::List(Vec::new())))
        }
        "thread_pool_size" => Some(Ok(InfoValue::Int(1))),
        "context_reductions" => Some(Ok(InfoValue::Int(CONTEXT_REDS))),
        _ => None,
    }
}

/// `+SDcpu N`, else `0` — this runtime has no dirty CPU scheduler pool.
fn dirty_cpu_schedulers(rt: &Runtime) -> i64 {
    rt.config()
        .emulator
        .dirty_cpu_schedulers
        .map(i64::from)
        .unwrap_or(0)
}

/// `+SDio N`, else `0` — this runtime has no dirty IO scheduler pool.
fn dirty_io_schedulers(rt: &Runtime) -> i64 {
    rt.config()
        .emulator
        .dirty_io_schedulers
        .map(i64::from)
        .unwrap_or(0)
}

/// `{unbound, unbound, …}`, one `unbound` atom per configured (normal) scheduler —
/// exactly the oracle's shape for a runtime that binds no scheduler to a processor.
fn scheduler_bindings(rt: &Runtime) -> InfoValue {
    let count = schedulers_count(rt);
    let count = usize::try_from(count).unwrap_or(0);
    InfoValue::Tupled(vec![InfoValue::Atom("unbound"); count])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::test_support::runtime;

    #[test]
    fn processor_and_scheduler_keys_are_self_consistent() {
        let rt = runtime();
        for key in [
            "logical_processors",
            "logical_processors_available",
            "logical_processors_online",
            "schedulers",
            "schedulers_online",
        ] {
            assert!(
                matches!(
                    try_value(key, &rt).unwrap().unwrap(),
                    InfoValue::Int(count) if count > 0
                ),
                "{key} must be a positive integer"
            );
        }
        assert_eq!(
            try_value("scheduler_id", &rt).unwrap().unwrap(),
            InfoValue::Int(1)
        );
        assert_eq!(
            try_value("thread_pool_size", &rt).unwrap().unwrap(),
            InfoValue::Int(1)
        );
        assert_eq!(
            try_value("scheduler_bind_type", &rt).unwrap().unwrap(),
            InfoValue::Atom("unbound")
        );
        assert_eq!(
            try_value("multi_scheduling_blockers", &rt).unwrap().unwrap(),
            InfoValue::List(Vec::new())
        );
        assert_eq!(
            try_value("normal_multi_scheduling_blockers", &rt)
                .unwrap()
                .unwrap(),
            InfoValue::List(Vec::new())
        );
        assert!(try_value("nonsense", &rt).is_none());
    }

    #[test]
    fn dirty_schedulers_default_to_zero_and_follow_the_flags() {
        let rt = runtime();
        assert_eq!(
            try_value("dirty_cpu_schedulers", &rt).unwrap().unwrap(),
            InfoValue::Int(0)
        );
        assert_eq!(
            try_value("dirty_io_schedulers", &rt).unwrap().unwrap(),
            InfoValue::Int(0)
        );

        let mut config = crate::config::RuntimeConfig::default();
        config.emulator.dirty_cpu_schedulers = Some(4);
        config.emulator.dirty_io_schedulers = Some(2);
        let rt = Runtime::new(config).expect("a runtime with dirty schedulers");
        assert_eq!(
            try_value("dirty_cpu_schedulers", &rt).unwrap().unwrap(),
            InfoValue::Int(4)
        );
        assert_eq!(
            try_value("dirty_io_schedulers", &rt).unwrap().unwrap(),
            InfoValue::Int(2)
        );
    }

    #[test]
    fn scheduler_bindings_has_one_unbound_per_scheduler() {
        let configured =
            Runtime::new(crate::config::RuntimeConfig::default().with_schedulers(3))
                .expect("a runtime with +S 3");
        assert_eq!(
            try_value("scheduler_bindings", &configured).unwrap().unwrap(),
            InfoValue::Tupled(vec![InfoValue::Atom("unbound"); 3])
        );
        // `+S 3` also selects the multi-scheduling answer.
        assert_eq!(
            try_value("multi_scheduling", &configured).unwrap().unwrap(),
            InfoValue::Atom("enabled")
        );
    }

    #[test]
    fn multi_scheduling_is_disabled_with_a_single_scheduler() {
        let configured =
            Runtime::new(crate::config::RuntimeConfig::default().with_schedulers(1))
                .expect("a runtime with +S 1");
        assert_eq!(
            try_value("multi_scheduling", &configured).unwrap().unwrap(),
            InfoValue::Atom("disabled")
        );
    }
}
