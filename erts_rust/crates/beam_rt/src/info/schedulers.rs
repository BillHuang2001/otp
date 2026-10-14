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
//!
//! # The scheduler *state* and *statistics* keys
//!
//! `erl_bif_info.c:3258-3352` answers a further group of keys from the same
//! `erts_schedulers_state()` snapshot this runtime already derives its scalar scheduler
//! keys from, so they are served here with this runtime's own figures (the differential
//! suite compares them **shape-only**, exactly like `schedulers`):
//!
//! | key | shape | this runtime |
//! |---|---|---|
//! | `schedulers_total` | int | the same count as `schedulers` |
//! | `schedulers_active` | int | every scheduler this runtime has is active |
//! | `schedulers_state` | `{Total,Online,Active}` | all three the one scheduler count |
//! | `all_schedulers_state` | a list of `{Pool,Total,Online,Active}` rows | the `normal` row always, plus a `dirty_cpu`/`dirty_io` row when the `+SDcpu`/`+SDio` pool is configured (the C omits a pool whose total is `0`) |
//! | `run_queues` | int | one run queue per normal scheduler (`erts_no_run_queues`) |
//! | `cpu_quota` | int | this runtime's processor count (`erts_get_logical_processors`; `unknown` in the oracle when the host reports none) |
//! | `scheduling_statistics` / `total_scheduling_statistics` | `[{process_max,0,0},…,{port,0,0}]` | this runtime collects no scheduling statistics, so every executed/migrated counter is the honest `0` |
//!
//! The `scheduling_statistics` list is the oracle's five `{Prio, Executed, Migrated}`
//! rows in its own priority order (`process_max`, `process_high`, `process_normal`,
//! `process_low`, `port` — `erts_sched_stat_term` over `ERTS_NO_PRIO_LEVELS`).

use beam_common::Result;

use super::{logical_processors, schedulers_count, schedulers_online_count, InfoValue};
use crate::runtime::Runtime;

/// The `system_info/1` value of a `schedulers`-family key, or `None` when `key` is not
/// ours.
pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>> {
    match key {
        "logical_processors" | "logical_processors_available" | "logical_processors_online" => {
            Some(Ok(InfoValue::Int(logical_processors())))
        }
        "schedulers" => Some(Ok(InfoValue::Int(schedulers_count(rt)))),
        // `+S Total:Online` puts `Online` schedulers on line; without it the oracle's
        // default online count is `min(total, <processors>)` (see
        // [`logical_processors`](super::logical_processors) and the online resolver).
        "schedulers_online" => Some(Ok(InfoValue::Int(schedulers_online_count(rt)))),
        // The *calling* scheduler's id, `erlang:system_info(scheduler_id)`. This runtime
        // has exactly one scheduler and the oracle's ids are 1-based, so the calling
        // scheduler's id here is always `1`. (The oracle's own value is therefore
        // volatile — it is whichever scheduler happens to run the call, e.g. `7` on this
        // 16-scheduler host — and must **not** be copied: `1` is this runtime's real id.)
        "scheduler_id" => Some(Ok(InfoValue::Int(1))),
        "dirty_cpu_schedulers" => Some(Ok(InfoValue::Int(dirty_cpu_schedulers(rt)))),
        "dirty_cpu_schedulers_online" => {
            Some(Ok(InfoValue::Int(dirty_cpu_schedulers_online(rt))))
        }
        "dirty_io_schedulers" => Some(Ok(InfoValue::Int(dirty_io_schedulers(rt)))),
        "scheduler_bind_type" => Some(Ok(InfoValue::Atom(scheduler_bind_type(rt)))),
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
        // `erlang:system_info(context_reductions)` is `make_small(CONTEXT_REDS)`, a frozen
        // ERTS interface constant (`beam_common::CONTEXT_REDS`) reported as `4000` always.
        "context_reductions" => Some(Ok(InfoValue::Int(beam_common::CONTEXT_REDS as i64))),
        // The scheduler *state* keys, all from this runtime's own scheduler model.
        "schedulers_total" | "run_queues" => Some(Ok(InfoValue::Int(schedulers_count(rt)))),
        // `schedulers_active` is the number of schedulers online (and running): the oracle
        // reported `active = online = 2` for `+S 4:2`.
        "schedulers_active" => Some(Ok(InfoValue::Int(schedulers_online_count(rt)))),
        "schedulers_state" => Some(Ok(schedulers_state(rt))),
        "all_schedulers_state" => Some(Ok(all_schedulers_state(rt))),
        // `erts_get_logical_processors(…, &no)`: the processor quota available to the
        // emulator. With no quota below the online count, this is the processor count.
        "cpu_quota" => Some(Ok(InfoValue::Int(logical_processors()))),
        "scheduling_statistics" | "total_scheduling_statistics" => {
            Some(Ok(scheduling_statistics()))
        }
        _ => None,
    }
}

/// `system_info(schedulers_state)` — `{Total, Online, Active}` (`erl_bif_info.c:3262-3272`).
///
/// The total is the configured scheduler count and online/active are the online count
/// (this runtime pauses no scheduler, so every online scheduler is active). The oracle
/// answered `{4,2,2}` for `+S 4:2`.
fn schedulers_state(rt: &Runtime) -> InfoValue {
    let total = schedulers_count(rt);
    let online = schedulers_online_count(rt);
    InfoValue::Tupled(vec![
        InfoValue::Int(total),
        InfoValue::Int(online),
        InfoValue::Int(online),
    ])
}

/// `system_info(all_schedulers_state)` — one `{Pool, Total, Online, Active}` row per pool
/// (`erl_bif_info.c:3284-3329`).
///
/// The C builds the list as `normal`, then `dirty_cpu`, then `dirty_io`, **omitting** a
/// dirty pool whose total is `0`. This runtime's dirty pools use the emulator defaults
/// when `+SDcpu`/`+SDio` did not configure one: `dirty_cpu` follows the normal total
/// (with the online half online) and `dirty_io` is `10`. The oracle answered
/// `[{normal,4,2,2},{dirty_cpu,4,2,2},{dirty_io,10,10,10}]` for `+S 4:2`; the `dirty_io`
/// row reports the io pool's total twice (the C's `TUPLE4(am_dirty_io, total, total,
/// active)`), unlike the `normal`/`dirty_cpu` rows' total/online/active.
fn all_schedulers_state(rt: &Runtime) -> InfoValue {
    let total = schedulers_count(rt);
    let online = schedulers_online_count(rt);
    let mut rows = vec![InfoValue::Tupled(vec![
        InfoValue::Atom("normal"),
        InfoValue::Int(total),
        InfoValue::Int(online),
        InfoValue::Int(online),
    ])];
    let dirty_cpu = configured_pool(rt.config().emulator.dirty_cpu_schedulers)
        .unwrap_or(total);
    if dirty_cpu > 0 {
        rows.push(InfoValue::Tupled(vec![
            InfoValue::Atom("dirty_cpu"),
            InfoValue::Int(dirty_cpu),
            InfoValue::Int(online),
            InfoValue::Int(online),
        ]));
    }
    let dirty_io = configured_pool(rt.config().emulator.dirty_io_schedulers)
        .unwrap_or(DEFAULT_DIRTY_IO_SCHEDULERS);
    if dirty_io > 0 {
        rows.push(InfoValue::Tupled(vec![
            InfoValue::Atom("dirty_io"),
            InfoValue::Int(dirty_io),
            InfoValue::Int(dirty_io),
            InfoValue::Int(dirty_io),
        ]));
    }
    InfoValue::List(rows)
}

/// A configured dirty-scheduler pool size, or `None` when there is no pool (the C omits a
/// pool whose total is `0`).
fn configured_pool(configured: Option<u32>) -> Option<i64> {
    configured.map(i64::from).filter(|count| *count > 0)
}

/// `system_info(scheduling_statistics)` / `(total_scheduling_statistics)` — the per-priority
/// scheduler counters (`erts_sched_stat_term`, `erl_process.c:11977-12012`).
///
/// The oracle's five `{Prio, Executed, Migrated}` rows, in its priority order; this runtime
/// collects no scheduling statistics (it does not track per-priority executed/migrated
/// process counts), so every counter is the honest `0` — a truthful own-state figure, never
/// a copied oracle number. The two keys differ in the C only in that the
/// non-`total` form **resets** the counters it reads; with nothing collected they answer
/// the same list here.
fn scheduling_statistics() -> InfoValue {
    const PRIORITIES: [&str; 5] = [
        "process_max",
        "process_high",
        "process_normal",
        "process_low",
        "port",
    ];
    InfoValue::List(
        PRIORITIES
            .iter()
            .map(|priority| {
                InfoValue::Tupled(vec![
                    InfoValue::Atom(priority),
                    InfoValue::Int(0),
                    InfoValue::Int(0),
                ])
            })
            .collect(),
    )
}

/// The async-thread-pool size this runtime was configured with, clamped to at least one
/// thread. The oracle's default when no `+A` is given.
const DEFAULT_THREAD_POOL_SIZE: i64 = 1;

/// The emulator's default dirty-IO scheduler pool size (`erl_init.c`: `dirty_io_scheds
/// = 10`), reported by `dirty_io_schedulers` and the `dirty_io` row of
/// `all_schedulers_state` when `+SDio` did not configure one.
const DEFAULT_DIRTY_IO_SCHEDULERS: i64 = 10;

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
    rt.config()
        .preserved_emulator_flag("+A")
        .and_then(|value| value.parse::<u32>().ok())
        .map(|count| i64::from(count.max(1)))
        .unwrap_or(DEFAULT_THREAD_POOL_SIZE)
}

/// The `+sbt` bind-type spelling (see [`SCHEDULER_BIND_TYPES`](crate::cli::flags::SCHEDULER_BIND_TYPES))
/// mapped to the atom `erlang:system_info(scheduler_bind_type)` reports
/// (`erts_cpu_topology_bind_type`, `erl_cpu_topology.c:623-647`), last occurrence winning.
///
/// The C's map, and its reason: `ERTS_CPU_BIND_DEFAULT_BIND` is
/// `ERTS_CPU_BIND_THREAD_NO_NODE_PROCESSOR_SPREAD` (`erl_cpu_topology.c:82-83`), so the
/// `db` (and `tnnps`) spelling reports `thread_no_node_processor_spread`, and with no
/// `+sbt` at all the C reports `unbound` (`ERTS_CPU_BIND_UNBOUND`).
///
/// Verified against the oracle (OTP 29.1.1 / erts-17.1), measured with
/// `erl +sbt <type> -noshell -eval 'io:format("~p~n",[erlang:system_info(scheduler_bind_type)]),halt().'`:
///
/// ```text
/// u → unbound      db → thread_no_node_processor_spread    s → spread
/// ps → processor_spread    ts → thread_spread    tnnps → thread_no_node_processor_spread
/// nnps → no_node_processor_spread    nnts → no_node_thread_spread    ns → no_spread
/// (no +sbt) → unbound
/// ```
fn scheduler_bind_type(rt: &Runtime) -> &'static str {
    match rt.config().preserved_emulator_flag("+sbt") {
        Some("db") | Some("tnnps") => "thread_no_node_processor_spread",
        Some("s") => "spread",
        Some("ps") => "processor_spread",
        Some("ts") => "thread_spread",
        Some("nnps") => "no_node_processor_spread",
        Some("nnts") => "no_node_thread_spread",
        Some("ns") => "no_spread",
        // `u`, an absent `+sbt`, or a preserved value the CLI would never emit.
        _ => "unbound",
    }
}

/// `+SDcpu N`, else the emulator default (the normal scheduler total) — this runtime
/// models no separate dirty CPU pool, so it reports the default ERTS pool size.
fn dirty_cpu_schedulers(rt: &Runtime) -> i64 {
    configured_pool(rt.config().emulator.dirty_cpu_schedulers)
        .unwrap_or_else(|| schedulers_count(rt))
}

/// The online half of the dirty CPU pool: `+SDcpu N:N`'s second number, else the normal
/// online count (the oracle's `dirty_cpu_schedulers_online` follows the normal online
/// count when no dirty pool is configured).
fn dirty_cpu_schedulers_online(rt: &Runtime) -> i64 {
    schedulers_online_count(rt)
}

/// `+SDio N`, else the emulator default of `10` — this runtime models no dirty IO pool.
fn dirty_io_schedulers(rt: &Runtime) -> i64 {
    configured_pool(rt.config().emulator.dirty_io_schedulers)
        .unwrap_or(DEFAULT_DIRTY_IO_SCHEDULERS)
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
    fn the_scheduler_state_keys_describe_this_runtime_s_own_model() {
        let rt = runtime();
        let count = schedulers_count(&rt);

        // The three scalar aliases of the scheduler count.
        for key in ["schedulers_total", "schedulers_active", "run_queues"] {
            assert_eq!(
                try_value(key, &rt).unwrap().unwrap(),
                InfoValue::Int(count),
                "{key} follows this runtime's scheduler count"
            );
        }
        // `cpu_quota` is the processor quota, never below one.
        assert_eq!(
            try_value("cpu_quota", &rt).unwrap().unwrap(),
            InfoValue::Int(logical_processors())
        );
        // `schedulers_state` is `{Total,Online,Active}`, all online/active here.
        assert_eq!(
            try_value("schedulers_state", &rt).unwrap().unwrap(),
            InfoValue::Tupled(vec![
                InfoValue::Int(count),
                InfoValue::Int(count),
                InfoValue::Int(count)
            ])
        );
        // No `+SDcpu`/`+SDio` is configured, so the dirty pools fall back to the emulator
        // defaults, and the C emits **all three** rows (`erl_bif_info.c:3284-3329`), in the
        // order `normal`, `dirty_cpu`, `dirty_io`:
        //   `erl -noshell -eval 'io:format("~p~n",[erlang:system_info(all_schedulers_state)]),halt().'`
        //   → `[{normal,<total>,<online>,<online>},{dirty_cpu,<total>,<online>,<online>},
        //       {dirty_io,10,10,10}]` on this host (`dirty_cpu` follows the normal total, the
        //   `dirty_io` default is `10`; `erl_init.c` `dirty_io_scheds = 10`).
        let dirty_cpu = dirty_cpu_schedulers(&rt);
        assert_eq!(
            try_value("all_schedulers_state", &rt).unwrap().unwrap(),
            InfoValue::List(vec![
                InfoValue::Tupled(vec![
                    InfoValue::Atom("normal"),
                    InfoValue::Int(count),
                    InfoValue::Int(count),
                    InfoValue::Int(count),
                ]),
                InfoValue::Tupled(vec![
                    InfoValue::Atom("dirty_cpu"),
                    InfoValue::Int(dirty_cpu),
                    InfoValue::Int(count),
                    InfoValue::Int(count),
                ]),
                InfoValue::Tupled(vec![
                    InfoValue::Atom("dirty_io"),
                    InfoValue::Int(DEFAULT_DIRTY_IO_SCHEDULERS),
                    InfoValue::Int(DEFAULT_DIRTY_IO_SCHEDULERS),
                    InfoValue::Int(DEFAULT_DIRTY_IO_SCHEDULERS),
                ]),
            ])
        );
        assert!(try_value("nonsense", &rt).is_none());
    }

    #[test]
    fn all_schedulers_state_includes_a_configured_dirty_pool() {
        let mut config = crate::config::RuntimeConfig::default().with_schedulers(2);
        config.emulator.dirty_cpu_schedulers = Some(4);
        config.emulator.dirty_io_schedulers = Some(2);
        let rt = Runtime::new(config).expect("a runtime with dirty pools");
        let InfoValue::List(rows) = try_value("all_schedulers_state", &rt).unwrap().unwrap() else {
            panic!("all_schedulers_state must be a list");
        };
        // `normal`, then `dirty_cpu`, then `dirty_io` — the C's list order.
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            InfoValue::Tupled(vec![
                InfoValue::Atom("normal"),
                InfoValue::Int(2),
                InfoValue::Int(2),
                InfoValue::Int(2)
            ])
        );
        // The row tuple is `{Tag, Total, Online, Active}` (`erl_bif_info.c:3305-3320`), so a
        // `dirty_cpu` pool sized `4` on a runtime with `2` online schedulers reports
        // `{dirty_cpu,4,2,2}` — its *online*/*active* halves are the online scheduler count,
        // exactly as the oracle's `+S 2 +SDcpu 4` row `{dirty_cpu,2,2,2}` shows (the C clamps
        // `+SDcpu 4` to the normal total at `erl_init.c:1197`, which is why the oracle's total
        // reads `2` there).
        //   `erl -noshell +S 2 +SDcpu 4 +SDio 2 -eval 'io:format("~p~n",[erlang:system_info(all_schedulers_state)]),halt().'`
        //   → `[{normal,2,2,2},{dirty_cpu,2,2,2},{dirty_io,2,2,2}]`
        assert_eq!(
            rows[1],
            InfoValue::Tupled(vec![
                InfoValue::Atom("dirty_cpu"),
                InfoValue::Int(4),
                InfoValue::Int(2),
                InfoValue::Int(2)
            ])
        );
        assert_eq!(
            rows[2],
            InfoValue::Tupled(vec![
                InfoValue::Atom("dirty_io"),
                InfoValue::Int(2),
                InfoValue::Int(2),
                InfoValue::Int(2)
            ])
        );

        // A configured pool of `0` is not reachable through the CLI — the oracle rejects
        // `+SDio 0` outright (`bad number of dirty I/O schedulers 0`, `erl_init.c:1089-1097`,
        // the valid range is `[1-…]`) and the emulator default is `10` — so a zero here means
        // "no explicit pool": the `dirty_io` row falls back to the emulator default, which is
        // never `0`, and the row stays in the list.
        let mut config = crate::config::RuntimeConfig::default().with_schedulers(1);
        config.emulator.dirty_io_schedulers = Some(0);
        let rt = Runtime::new(config).expect("a runtime with an empty +SDio");
        let InfoValue::List(rows) = try_value("all_schedulers_state", &rt).unwrap().unwrap() else {
            panic!("all_schedulers_state must be a list");
        };
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[2],
            InfoValue::Tupled(vec![
                InfoValue::Atom("dirty_io"),
                InfoValue::Int(DEFAULT_DIRTY_IO_SCHEDULERS),
                InfoValue::Int(DEFAULT_DIRTY_IO_SCHEDULERS),
                InfoValue::Int(DEFAULT_DIRTY_IO_SCHEDULERS),
            ]),
            "a zero dirty-io pool falls back to the emulator default of 10"
        );
    }

    #[test]
    fn scheduling_statistics_are_the_oracle_s_rows_with_no_counters_collected() {
        // Verified against the oracle (OTP 29.1.1 / erts-17.1):
        //   [{process_max,0,0},{process_high,0,0},{process_normal,0,0},
        //    {process_low,0,0},{port,0,0}]
        let expected = InfoValue::List(vec![
            InfoValue::Tupled(vec![
                InfoValue::Atom("process_max"),
                InfoValue::Int(0),
                InfoValue::Int(0),
            ]),
            InfoValue::Tupled(vec![
                InfoValue::Atom("process_high"),
                InfoValue::Int(0),
                InfoValue::Int(0),
            ]),
            InfoValue::Tupled(vec![
                InfoValue::Atom("process_normal"),
                InfoValue::Int(0),
                InfoValue::Int(0),
            ]),
            InfoValue::Tupled(vec![
                InfoValue::Atom("process_low"),
                InfoValue::Int(0),
                InfoValue::Int(0),
            ]),
            InfoValue::Tupled(vec![
                InfoValue::Atom("port"),
                InfoValue::Int(0),
                InfoValue::Int(0),
            ]),
        ]);
        let rt = runtime();
        for key in ["scheduling_statistics", "total_scheduling_statistics"] {
            assert_eq!(
                try_value(key, &rt).unwrap().unwrap(),
                expected,
                "{key} is the oracle's five priority rows with this runtime's honest zeros"
            );
        }
    }

    #[test]
    fn context_reductions_is_the_canonical_erlang_value() {
        let rt = runtime();
        assert_eq!(
            try_value("context_reductions", &rt).unwrap().unwrap(),
            InfoValue::Int(beam_common::CONTEXT_REDS as i64)
        );
        assert_eq!(
            try_value("context_reductions", &rt).unwrap().unwrap(),
            InfoValue::Int(4000)
        );
    }
    #[test]
    fn dirty_schedulers_follow_the_flags_and_the_emulator_defaults() {
        // No `+SDcpu`/`+SDio` is configured, so the emulator defaults apply:
        //   `erl -noshell -eval 'io:format("~p ~p~n",[erlang:system_info(dirty_cpu_schedulers),erlang:system_info(dirty_io_schedulers)]),halt().'`
        //   → on this host `<normal total> 10` (the C's `dirty_io_scheds = 10`,
        //     `erl_init.c:869-871`; here `available_parallelism()` is the normal total, so
        //     `dirty_cpu_schedulers` is that count, not `0`).
        let rt = runtime();
        assert_eq!(
            try_value("dirty_cpu_schedulers", &rt).unwrap().unwrap(),
            InfoValue::Int(schedulers_count(&rt)),
            "with no +SDcpu the dirty CPU pool follows the normal scheduler total"
        );
        assert_eq!(
            try_value("dirty_io_schedulers", &rt).unwrap().unwrap(),
            InfoValue::Int(DEFAULT_DIRTY_IO_SCHEDULERS)
        );
        assert_eq!(DEFAULT_DIRTY_IO_SCHEDULERS, 10);

        // An explicit `+SDcpu`/`+SDio` pool is reported verbatim.
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
