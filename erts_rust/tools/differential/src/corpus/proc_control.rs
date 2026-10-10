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

//! Contract **A.7** — process control: `monitor`/`demonitor`, the `exit/2` target
//! taxonomy, process registration (`register`/`unregister`/`whereis`) and the run-queue
//! priority surface (`process_flag/2`).
//!
//! Every golden is a **stable** value or shape: the reference/pid of a monitor is never
//! rendered (it differs per VM start), a `DOWN` message is matched and its fields compared
//! to local bindings, and a raised `badarg` is observed through the *class* of the caught
//! reason (`element(1, catch E)` is the atom `EXIT`) rather than a stacktrace.
//!
//! # Deliberately NOT pinned here
//!
//! * **`erlang:exit(self(), normal)`** — the oracle *ignores* a `normal` exit signal sent
//!   to the caller (returns `true`), while the Rust runtime raises `exit:normal`. That is a
//!   plain differing result (a `mismatch`), so it is a handoff to the runtime, not a case.
//! * **`monitor/2` of an already-dead pid followed by a `receive`** — the DOWN reason is
//!   `noproc` only when the process died *before* the monitor call, so the observable races
//!   the scheduler; the deterministic variant (monitor a blocked process, then kill it) is
//!   pinned instead.
//! * **`timer:sleep/1`** blocks in the Rust runtime (a capability gap), so the cases use
//!   `receive … after N ->` for ordering.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.7 case: evaluate `expr` under both runtimes and pin the oracle's `~w` rendering.
fn case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::OtpApps],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// One A.7 case whose expression raises (`golden` is the probe's `exception …` line).
fn raises(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        golden: Golden {
            stdout: Some(golden),
            exit_code: 1,
        },
        ..case(id, description, expr, golden)
    }
}

/// The A.7 process-control cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- monitor / demonitor -----------------------------------------------------------
        case(
            "proc_control/demonitor_returns_true",
            "erlang:demonitor/1 returns true for an active monitor",
            "begin M = erlang:monitor(process, self()), erlang:demonitor(M) end",
            "ok true",
        ),
        case(
            "proc_control/demonitor_flush_returns_true",
            "erlang:demonitor/2 with `flush` returns true",
            "begin P = spawn(fun() -> receive stop -> ok end end), \
             M = erlang:monitor(process, P), erlang:demonitor(M, [flush]) end",
            "ok true",
        ),
        case(
            "proc_control/monitor_of_finished_process_is_reference",
            "monitoring an exited process still returns a reference",
            "begin P = spawn(fun() -> ok end), is_reference(erlang:monitor(process, P)) end",
            "ok true",
        ),
        case(
            "proc_control/down_message_on_killed_process",
            "killing a monitored blocked process delivers {'DOWN', Ref, process, Pid, killed}",
            "begin P = spawn(fun() -> receive stop -> ok end end), \
             M = erlang:monitor(process, P), erlang:exit(P, kill), \
             receive {DOWN, Ref, process, Q, Reason} -> {Ref =:= M, Q =:= P, Reason} \
             after 2000 -> timeout end end",
            "ok {true,true,killed}",
        ),
        case(
            "proc_control/down_message_on_exit_kill",
            "exit(Pid, kill) on a monitored process delivers the ref and a `killed` reason",
            "begin P = spawn(fun() -> receive stop -> ok end end), \
             M = erlang:monitor(process, P), exit(P, kill), \
             receive {DOWN, Ref, process, Q, Reason} -> {Ref =:= M, Reason} \
             after 2000 -> timeout end end",
            "ok {true,killed}",
        ),
        // --- exit/2 and exit/1 -------------------------------------------------------------
        case(
            "proc_control/exit_kill_live_process",
            "erlang:exit(Pid, kill) is accepted for a live blocked process",
            "begin P = spawn(fun() -> receive stop -> ok end end), erlang:exit(P, kill), ok end",
            "ok ok",
        ),
        case(
            "proc_control/exit_normal_to_live_process",
            "erlang:exit(Pid, normal) returns true for a non-linked process",
            "begin P = spawn(fun() -> receive stop -> ok end end), erlang:exit(P, normal), ok end",
            "ok ok",
        ),
        case(
            "proc_control/exit_kill_returns_true",
            "erlang:exit/2 returns true (the observable of the kill signal)",
            "begin P = spawn(fun() -> receive stop -> ok end end), erlang:exit(P, kill) end",
            "ok true",
        ),
        case(
            "proc_control/exit_non_id_target_is_badarg",
            "erlang:exit/2 with a non-id target (an integer) raises error:badarg",
            "element(1, catch erlang:exit(42, some_reason))",
            "ok 'EXIT'",
        ),
        case(
            "proc_control/exit_unknown_registered_name_is_badarg",
            "erlang:exit/2 with an unregistered atom name raises error:badarg",
            "element(1, catch erlang:exit(no_such_registered_name, some_reason))",
            "ok 'EXIT'",
        ),
        case(
            "proc_control/exit_1_is_catchable",
            "the exit/1 class is catchable (its caught shape is an EXIT tuple)",
            "element(1, catch exit(some_reason))",
            "ok 'EXIT'",
        ),
        // --- registration ------------------------------------------------------------------
        raises(
            "proc_control/unregister_unregistered_name_is_badarg",
            "unregister/1 of a name that is not registered raises error:badarg",
            "unregister(a_name_that_is_not_registered)",
            "exception error badarg",
        ),
        case(
            "proc_control/register_non_atom_name_is_badarg",
            "register/2 with a non-atom name raises error:badarg",
            "element(1, catch register(42, self()))",
            "ok 'EXIT'",
        ),
        case(
            "proc_control/register_then_whereis_is_self",
            "register/2 then whereis/1 resolves the name back to the registering process",
            "begin register(proc_ctl_self_name, self()), \
             whereis(proc_ctl_self_name) =:= self() end",
            "ok true",
        ),
        case(
            "proc_control/unregister_then_whereis_is_undefined",
            "unregister/1 makes whereis/1 return undefined",
            "begin register(proc_ctl_gone, self()), unregister(proc_ctl_gone), \
             whereis(proc_ctl_gone) end",
            "ok undefined",
        ),
        case(
            "proc_control/register_duplicate_name_is_badarg",
            "registering an already-registered name raises error:badarg",
            "begin register(proc_ctl_dup, self()), \
             element(1, catch register(proc_ctl_dup, \
                 spawn(fun() -> receive stop -> ok end end))) end",
            "ok 'EXIT'",
        ),
        // --- the run-queue priority surface ------------------------------------------------
        case(
            "proc_control/priority_high_then_max",
            "process_flag/2 returns the previous priority ({ok, high} when lowering to max)",
            "begin process_flag(priority, high), {ok, process_flag(priority, max)} end",
            "ok {ok,high}",
        ),
        case(
            "proc_control/priority_walks_all_four_levels",
            "process_flag/2 walks low/normal/high/max, returning the previous level each time",
            "begin A = process_flag(priority, low), B = process_flag(priority, normal), \
             D = process_flag(priority, high), E = process_flag(priority, max), \
             {A, B, D, E} end",
            "ok {normal,low,normal,high}",
        ),
        case(
            "proc_control/priority_bogus_is_badarg",
            "process_flag/2 with an unknown priority raises error:badarg",
            "element(1, catch process_flag(priority, bogus))",
            "ok 'EXIT'",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The monitor / `demonitor` / DOWN-message cases.
    const MONITOR_CASES: &[&str] = &[
        "proc_control/demonitor_returns_true",
        "proc_control/demonitor_flush_returns_true",
        "proc_control/monitor_of_finished_process_is_reference",
        "proc_control/down_message_on_killed_process",
        "proc_control/down_message_on_exit_kill",
    ];

    /// The `exit/1,2` cases.
    const EXIT_CASES: &[&str] = &[
        "proc_control/exit_kill_live_process",
        "proc_control/exit_normal_to_live_process",
        "proc_control/exit_kill_returns_true",
        "proc_control/exit_non_id_target_is_badarg",
        "proc_control/exit_unknown_registered_name_is_badarg",
        "proc_control/exit_1_is_catchable",
    ];

    /// The `register`/`unregister`/`whereis` cases.
    const REGISTRATION_CASES: &[&str] = &[
        "proc_control/unregister_unregistered_name_is_badarg",
        "proc_control/register_non_atom_name_is_badarg",
        "proc_control/register_then_whereis_is_self",
        "proc_control/unregister_then_whereis_is_undefined",
        "proc_control/register_duplicate_name_is_badarg",
    ];

    /// The run-queue priority cases.
    const PRIORITY_CASES: &[&str] = &[
        "proc_control/priority_high_then_max",
        "proc_control/priority_walks_all_four_levels",
        "proc_control/priority_bogus_is_badarg",
    ];

    fn case<'a>(cases: &'a [Case], id: &str) -> &'a Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the proc_control group"))
    }

    #[test]
    fn every_case_is_a_plain_a7_eval_case_with_no_extra_argv() {
        let cases = group();
        let cases = cases.as_slice();
        for id in MONITOR_CASES
            .iter()
            .chain(EXIT_CASES)
            .chain(REGISTRATION_CASES)
            .chain(PRIORITY_CASES)
        {
            let case = case(cases, id);
            assert_eq!(case.contracts, &[Contract::OtpApps], "case {id}");
            assert!(
                matches!(case.kind, CaseKind::Eval { .. }),
                "case {id} must be an Eval case"
            );
            assert!(case.extra_argv().is_empty(), "case {id} takes no extra argv");
            assert!(case.env().is_empty(), "case {id} sets no environment");
        }
        assert_eq!(
            cases.len(),
            MONITOR_CASES.len()
                + EXIT_CASES.len()
                + REGISTRATION_CASES.len()
                + PRIORITY_CASES.len()
        );
    }

    #[test]
    fn goldens_never_pin_a_volatile_pid_or_reference() {
        for case in group() {
            let golden = case.golden.stdout.expect("every case asserts stdout");
            assert!(
                !golden.contains("<0."),
                "case {}: the golden pins an unstable pid: {golden:?}",
                case.id
            );
            assert!(
                !golden.contains("#Ref<"),
                "case {}: the golden pins an unstable reference: {golden:?}",
                case.id
            );
        }
    }

    #[test]
    fn the_raised_shapes_are_observed_by_class_not_by_stacktrace() {
        let cases = group();
        let cases = cases.as_slice();
        // A caught reason is observed through `element(1, catch …)` — the atom `EXIT` —
        // never a stacktrace, which would embed a module/line/pid.
        for id in [
            "proc_control/exit_non_id_target_is_badarg",
            "proc_control/exit_unknown_registered_name_is_badarg",
            "proc_control/exit_1_is_catchable",
            "proc_control/register_non_atom_name_is_badarg",
            "proc_control/register_duplicate_name_is_badarg",
            "proc_control/priority_bogus_is_badarg",
        ] {
            let case = case(cases, id);
            assert_eq!(case.golden.stdout, Some("ok 'EXIT'"), "case {id}");
            assert_eq!(case.golden.exit_code, 0, "case {id}");
            let expr = case.expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("element(1, catch "),
                "case {id}: a caught reason must be reduced to its class: {expr:?}"
            );
        }
        // The one *uncaught* raise is observed through the probe's `exception` line.
        let case = case(cases, "proc_control/unregister_unregistered_name_is_badarg");
        assert_eq!(case.golden.stdout, Some("exception error badarg"));
        assert_eq!(case.golden.exit_code, 1);
    }

    #[test]
    fn the_priority_cases_cover_all_four_levels_and_the_invalid_one() {
        let cases = group();
        let cases = cases.as_slice();
        let joined: String = PRIORITY_CASES
            .iter()
            .filter_map(|id| case(cases, id).expr())
            .collect::<Vec<_>>()
            .join(" ");
        for level in ["low", "normal", "high", "max", "bogus"] {
            assert!(
                joined.contains(&format!("priority, {level}")),
                "no priority case probes `{level}`"
            );
        }
    }
}
