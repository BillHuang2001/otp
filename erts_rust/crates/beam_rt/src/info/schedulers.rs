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
//! counts come from [`std::thread::available_parallelism`] — which is exactly the
//! oracle's own *default* rule for [`schedulers`](schedulers_count), not a fabricated
//! constant — an explicit `+S N` wins for the normal scheduler count, and the dirty
//! schedulers default to `0` (this runtime has none) unless `+SDcpu`/`+SDio` configured
//! some. The bind keys are the honest answer for a runtime that never binds a scheduler
//! to a processor, and `thread_pool_size` follows the real `+A N` on the command line.
//!
//! | key | shape |
//! |---|---|
//! | `logical_processors` / `_available` / `_online` | int (`available_parallelism`) |
//! | `schedulers` | int (`+S N`, else the processor count — the oracle's own default rule) |
//! | `schedulers_online` | int (all schedulers are online here) |
//! | `scheduler_id` | int (`1` — the id of this runtime's only scheduler) |
//! | `dirty_cpu_schedulers` / `_online` | int (`+SDcpu` or `0`) |
//! | `dirty_io_schedulers` | int (`+SDio` or `0`) |
//! | `scheduler_bind_type` | atom, `unbound` |
//! | `scheduler_bindings` | `{unbound, unbound, …}`, one per scheduler |
//! | `multi_scheduling` | atom, `enabled`/`disabled` |
//! | `multi_scheduling_blockers` / `normal_multi_scheduling_blockers` | `[]` |
//! | `thread_pool_size` | int (`+A N` clamped to `>= 1`, else `1`) |
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
///
/// **TODO (de-duplication): this local copy is to be REPLACED.** The same quantum is
/// declared independently as this constant, `beam_io::port_task::CONTEXT_REDS` and
/// `beam_process::scheduler::DEFAULT_REDUCTIONS_PER_SLICE`; the three are not linked at
/// compile time. A single canonical owner is being established in the `beam_common`
/// foundation crate (the workspace's lowest common ancestor, a dependency of every crate
/// here); once that lands, replace this declaration — and the `beam_io`/`beam_process`
/// copies — with a reference/re-export to the canonical constant. Until then the value
/// below must stay `4000` (identical to the canonical one) so behaviour is unchanged.
const CONTEXT_REDS: i64 = 4000;

/// The `system_info/1` value of a `schedulers`-family key, or `None` when `key` is not
/// ours.
pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>> {
    match key {
        "logical_processors" | "logical_processors_available" | "logical_processors_online" => {
            Some(Ok(InfoValue::Int(logical_processors())))
        }
        "schedulers" => Some(Ok(InfoValue::Int(schedulers_count(rt)))),
        // Every scheduler this runtime has is online: there is no online/offline model
        // (the oracle may report fewer online than configured under load, so
        // `schedulers_online == schedulers` is the honest answer for this model).
        "schedulers_online" => Some(Ok(InfoValue::Int(schedulers_count(rt)))),
        // The *calling* scheduler's id, `erlang:system_info(scheduler_id)`. This runtime
        // has exactly one scheduler and the oracle's ids are 1-based, so the calling
        // scheduler's id here is always `1`. (The oracle's own value is therefore
        // volatile — it is whichever scheduler happens to run the call, e.g. `7` on this
        // 16-scheduler host — and must **not** be copied: `1` is this runtime's real id.)
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
        "thread_pool_size" => Some(Ok(InfoValue::Int(thread_pool_size(rt)))),
        "context_reductions" => Some(Ok(InfoValue::Int(CONTEXT_REDS))),
        _ => None,
    }
}

/// The async-thread-pool size this runtime was configured with, clamped to at least one
/// thread. The oracle's default when no `+A` is given.
const DEFAULT_THREAD_POOL_SIZE: i64 = 1;

/// `erlang:system_info(thread_pool_size)` — the async-thread-pool size the emulator was
/// started with.
///
/// The oracle reports the pool `+A N` configures, clamped to a minimum of one thread:
/// no `+A` ⇒ `1` (its default), `+A 4` ⇒ `4`, `+A 0` ⇒ `1` (verified against OTP 29.1.1 /
/// erts-17.1). This runtime runs no async thread (`Runtime` is `!Send`), but the
/// *configured* pool size is real state, not a compiled-in constant: the CLI validates
/// `+A N` and preserves the `(switch, value)` pair `["+A", "<N>"]` verbatim in
/// [`extra_emulator_flags`](beam_common::RuntimeConfig::extra_emulator_flags)
/// (`crate::cli`'s `+A` arm), so this answers from that pair and tracks the command line.
///
/// Every writer of `extra_emulator_flags` (`preserve_emulator_flag`) pushes exactly two
/// tokens, so the list is a flat `(switch, value)` sequence and stepping by two reads a
/// switch at an even offset with its value at the odd one; a repeated `+A` follows the
/// oracle's last-one-wins order. A malformed or truncated entry keeps the default —
/// never a panic.
fn thread_pool_size(rt: &Runtime) -> i64 {
    let mut size = DEFAULT_THREAD_POOL_SIZE;
    for pair in rt.config().emulator.extra_emulator_flags.chunks_exact(2) {
        if pair[0] == "+A" {
            if let Ok(count) = pair[1].parse::<u32>() {
                size = i64::from(count.max(1));
            }
        }
    }
    size
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

    /// The runtime built from `argv` through the real A.6 parser.
    fn runtime_from(argv: &[&str]) -> Runtime {
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
        let config = crate::cli::parse(&argv).expect("the argv parses");
        Runtime::new(config).expect("a runtime for the argv")
    }

    fn thread_pool_size_of(rt: &Runtime) -> InfoValue {
        try_value("thread_pool_size", rt).unwrap().unwrap()
    }

    #[test]
    fn thread_pool_size_follows_the_configured_async_pool() {
        // Verified against the oracle (OTP 29.1.1 / erts-17.1):
        //   `erl -noshell …` → 1, `erl +A 4 …` → 4, `erl +A 0 …` → 1.
        assert_eq!(
            thread_pool_size_of(&runtime_from(&["+A", "4"])),
            InfoValue::Int(4),
            "+A 4 reports the configured async-thread pool"
        );
        assert_eq!(
            thread_pool_size_of(&runtime()),
            InfoValue::Int(1),
            "no +A reports the oracle's default of one thread"
        );
        assert_eq!(
            thread_pool_size_of(&runtime_from(&["+A", "0"])),
            InfoValue::Int(1),
            "+A 0 is clamped to one thread, exactly as the oracle clamps it"
        );
        // A repeated switch follows the oracle's last-one-wins order.
        assert_eq!(
            thread_pool_size_of(&runtime_from(&["+A", "2", "+A", "4"])),
            InfoValue::Int(4)
        );
    }

    #[test]
    fn thread_pool_size_never_panics_on_a_malformed_preserved_flag() {
        // A truncated pair (the value token missing) keeps the default.
        let mut config = crate::config::RuntimeConfig::default();
        config.emulator.extra_emulator_flags = vec!["+A".to_string()];
        let rt = Runtime::new(config).expect("a runtime with a truncated +A");
        assert_eq!(thread_pool_size_of(&rt), InfoValue::Int(1));

        // A value the CLI would never preserve is ignored, not parsed-and-panicked on.
        let mut config = crate::config::RuntimeConfig::default();
        config.emulator.extra_emulator_flags =
            vec!["+A".to_string(), "not-a-number".to_string()];
        let rt = Runtime::new(config).expect("a runtime with a non-numeric +A value");
        assert_eq!(thread_pool_size_of(&rt), InfoValue::Int(1));
    }
}
