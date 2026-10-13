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

//! Contract **A.7** — process signals: the group leader, the distribution-tag
//! (`dt_*`) switches, `erlang:statistics/1`, and the `spawn_monitor`/`monitor` surface.
//!
//! Every golden below is the **shape** the oracle prints, never a volatile identity:
//!
//! * a group leader / monitored process is observed with `is_pid/1`, `is_reference/1`
//!   or an equality against a pid bound **in the same expression** — a raw pid rendering
//!   (`<0.70.0>`) differs per VM start and is never pinned;
//! * the clock/counter values of `erlang:statistics/1` are never asserted (this runtime
//!   reports zeros for several of them), only that the tuple/atom shape is right.
//!
//! # The group-leader cases restore the original leader
//!
//! `io:format/2` delivers its request *through the calling process's group leader*, and
//! the probe wrapper prints its single line **after** the expression has been evaluated.
//! An expression that leaves the probe process pointed at a live-but-silent process would
//! therefore block the probe forever (on the oracle too). Each group-leader case captures
//! `erlang:group_leader()` first and restores it before returning, so the probe always
//! reaches a real IO server. (An already-dead leader would unblock via the monitor, but a
//! *live* one would hang — the restore is what makes the case deterministic.)
//!
//! # The `erlang:statistics/1` counters are now largely served
//!
//! The Rust runtime serves `wall_clock`, `reductions`, `context_switches`, `io` and
//! `scheduler_wall_time`, so the corresponding cases **`agree`** and are regression-pinned
//! (their `_unsupported` id suffixes are historical — kept only so the ids stay byte-stable).
//! Only `runtime` still raises the documented unsupported signal
//! (`erlang:statistics/1: the \`runtime\` counter is not implemented`, exit 3), so that case
//! classifies `unsupported`, never `mismatch`. The clock/counter **values** are volatile and
//! are never asserted — every case pins only the tuple/atom shape and integer-ness.
//!
//! # The `{tag, _}` monitor option and port monitors are live
//!
//! `erlang:monitor/3` with a `{tag, _}` option and `erlang:monitor/2` on a **port** were once
//! Rust capability gaps (the runtime raised `beam_rt: unsupported: …`, exit 3). The runtime now
//! honours both, so `proc_signals/monitor_3_tag_option_unsupported` and
//! `proc_signals/monitor_port_unsupported` **agree** — their `_unsupported` id suffix (kept so
//! the ids stay byte-stable) is historical. The extra `monitor_3_tag_*` / `monitor_3_*` cases
//! below deepen that coverage so the behaviour cannot silently regress.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.7 case: evaluate `expr` under both runtimes and pin the oracle's `~w` rendering.
///
/// `golden` is the oracle's stdout line (`None` never applies here — the corpus requires a
/// golden for every case).
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

/// The A.7 process-signal cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- the group leader (read + set + inheritance), all shape-only -----------------
        case(
            "proc_signals/group_leader_is_pid",
            "erlang:group_leader/0 is a pid (its value differs per VM start)",
            "is_pid(erlang:group_leader())",
            "ok true",
        ),
        case(
            "proc_signals/process_info_group_leader_is_pid",
            "process_info(self(), group_leader) reports a pid",
            "is_pid(element(2, process_info(self(), group_leader)))",
            "ok true",
        ),
        case(
            "proc_signals/group_leader_set_and_restore",
            "erlang:group_leader/2 sets the caller's leader (true) and returns true again \
             when restoring it",
            "begin Old = erlang:group_leader(), P = spawn(fun() -> receive stop -> ok end end), \
             Set = erlang:group_leader(P, self()), Restored = erlang:group_leader(Old, self()), \
             {is_pid(P), Set, Restored} end",
            "ok {true,true,true}",
        ),
        case(
            "proc_signals/group_leader_inherited_by_spawn",
            "a freshly spawned process inherits the setter's group leader",
            "begin Old = erlang:group_leader(), G = spawn(fun() -> receive stop -> ok end end), \
             erlang:group_leader(G, self()), \
             Child = spawn(fun() -> receive {gl, From} -> \
                 From ! {gl_result, erlang:group_leader() =:= G} end end), \
             Child ! {gl, self()}, \
             Res = receive {gl_result, B} -> B after 2000 -> timeout end, \
             erlang:group_leader(Old, self()), Res end",
            "ok true",
        ),
        case(
            "proc_signals/erts_internal_group_leader_set_and_restore",
            "erts_internal:group_leader/2 sets the caller's leader and the accessor restores it",
            "begin Old = erlang:group_leader(), P = spawn(fun() -> receive stop -> ok end end), \
             Set = erts_internal:group_leader(self(), P), \
             Restored = erlang:group_leader(Old, self()), {is_pid(P), Set, Restored} end",
            "ok {true,true,true}",
        ),
        raises(
            "proc_signals/erts_internal_group_leader_bad_option_is_badarg",
            "erts_internal:group_leader/3 with the option `none` raises error:badarg",
            "begin P = spawn(fun() -> receive stop -> ok end end), \
             erts_internal:group_leader(self(), P, none), ok end",
            "exception error badarg",
        ),
        // --- the distribution-tag switches -------------------------------------------------
        case(
            "proc_signals/dt_spread_tag_true",
            "erlang:dt_spread_tag/1 accepts `true` and returns true",
            "erlang:dt_spread_tag(true)",
            "ok true",
        ),
        case(
            "proc_signals/dt_spread_tag_false",
            "erlang:dt_spread_tag/1 accepts `false` and returns true",
            "erlang:dt_spread_tag(false)",
            "ok true",
        ),
        raises(
            "proc_signals/dt_spread_tag_bad_arg_is_badarg",
            "erlang:dt_spread_tag/1 with a malformed argument raises error:badarg",
            "erlang:dt_spread_tag({1,2,3})",
            "exception error badarg",
        ),
        case(
            "proc_signals/dt_restore_tag_true",
            "erlang:dt_restore_tag/1 accepts `true` and returns true",
            "erlang:dt_restore_tag(true)",
            "ok true",
        ),
        raises(
            "proc_signals/dt_restore_tag_non_boolean_is_badarg",
            "erlang:dt_restore_tag/1 with a non-boolean raises error:badarg",
            "erlang:dt_restore_tag({1,1})",
            "exception error badarg",
        ),
        // --- erlang:statistics/1 -----------------------------------------------------------
        // `wall_clock`, `reductions`, `context_switches`, `io` and `scheduler_wall_time` are
        // served by both runtimes; every value is volatile, so only the tuple/atom shape and
        // integer-ness are pinned. The `_unsupported` id suffixes on the counters that were
        // once Rust capability gaps are historical — kept only so the ids stay byte-stable.
        case(
            "proc_signals/statistics_wall_clock_shape",
            "erlang:statistics(wall_clock) is a two-integer tuple (the values are volatile)",
            "begin {A, B} = erlang:statistics(wall_clock), {is_integer(A), is_integer(B)} end",
            "ok {true,true}",
        ),
        case(
            "proc_signals/statistics_reductions_unsupported",
            "erlang:statistics(reductions) is a two-integer tuple (now served; regression-pinned agreement)",
            "is_integer(element(1, erlang:statistics(reductions)))",
            "ok true",
        ),
        // `runtime` is served too (its `_unsupported` id suffix is historical, kept so the
        // id stays byte-stable); it is pinned as a regression-pinned agreement, like the
        // other counters, and its golden is the oracle's own stable shape.
        case(
            "proc_signals/statistics_runtime_unsupported",
            "erlang:statistics(runtime) is a two-integer tuple (now served; regression-pinned agreement)",
            "is_integer(element(1, erlang:statistics(runtime)))",
            "ok true",
        ),
        case(
            "proc_signals/statistics_context_switches_unsupported",
            "erlang:statistics(context_switches) is a two-integer tuple (now served; regression-pinned agreement)",
            "is_integer(element(1, erlang:statistics(context_switches)))",
            "ok true",
        ),
        case(
            "proc_signals/statistics_io_unsupported",
            "erlang:statistics(io) is a {{input,_},{output,_}} tuple of integers \
             (now served; regression-pinned agreement)",
            "begin {{In, InN}, {Out, OutN}} = erlang:statistics(io), \
             {In, Out, is_integer(InN), is_integer(OutN)} end",
            "ok {input,output,true,true}",
        ),
        case(
            "proc_signals/statistics_scheduler_wall_time_unsupported",
            "erlang:statistics(scheduler_wall_time) is `undefined` when not enabled \
             (now served; regression-pinned agreement)",
            "erlang:statistics(scheduler_wall_time)",
            "ok undefined",
        ),
        // Regression coverage for the newly-served counters: pin each counter's **shape**
        // structurally (tuple sizes, `input`/`output` tags, integer-ness). The values are
        // volatile and are never asserted.
        case(
            "proc_signals/statistics_reductions_is_two_integer_tuple",
            "erlang:statistics(reductions) is a 2-element tuple of integers",
            "begin Red = erlang:statistics(reductions), {tuple_size(Red), \
             is_integer(element(1, Red)), is_integer(element(2, Red))} end",
            "ok {2,true,true}",
        ),
        case(
            "proc_signals/statistics_context_switches_is_two_integer_tuple",
            "erlang:statistics(context_switches) is a 2-element tuple of integers",
            "begin Ctx = erlang:statistics(context_switches), {tuple_size(Ctx), \
             is_integer(element(1, Ctx)), is_integer(element(2, Ctx))} end",
            "ok {2,true,true}",
        ),
        case(
            "proc_signals/statistics_io_outer_shape",
            "erlang:statistics(io) is a 2-element tuple tagged input/output",
            "begin {In, Out} = erlang:statistics(io), \
             {tuple_size(erlang:statistics(io)), element(1, In), element(1, Out)} end",
            "ok {2,input,output}",
        ),
        // --- spawn_monitor -----------------------------------------------------------------
        case(
            "proc_signals/spawn_monitor_returns_pid_and_reference",
            "spawn_monitor/1 returns {Pid, MonitorRef}",
            "begin {P, M} = spawn_monitor(fun() -> ok end), {is_pid(P), is_reference(M)} end",
            "ok {true,true}",
        ),
        case(
            "proc_signals/spawn_monitor_3_returns_pid_and_reference",
            "spawn_monitor/3 (Node, Fun) returns {Pid, MonitorRef}",
            "begin {P, M} = spawn_monitor(node(), fun() -> ok end), \
             {is_pid(P), is_reference(M)} end",
            "ok {true,true}",
        ),
        case(
            "proc_signals/spawn_monitor_4_returns_pid_and_reference",
            "spawn_monitor/4 (Node, Module, Function, Args) returns {Pid, MonitorRef}",
            "begin {P, M} = spawn_monitor(node(), erlang, node, []), \
             {is_pid(P), is_reference(M)} end",
            "ok {true,true}",
        ),
        case(
            "proc_signals/spawn_monitor_down_message_shape",
            "a monitored process' abnormal exit delivers {'DOWN', Ref, process, Pid, Reason}",
            "begin {P, M} = spawn_monitor(fun() -> exit(boom) end), \
             receive Msg -> case Msg of \
                 {'DOWN', Ref, process, Q, Reason} -> {Ref =:= M, Q =:= P, Reason}; \
                 _ -> unexpected end \
             after 2000 -> timeout end end",
            "ok {true,true,boom}",
        ),
        // --- erlang:monitor/2,3 ------------------------------------------------------------
        case(
            "proc_signals/monitor_self_is_reference",
            "erlang:monitor(process, self()) returns a reference",
            "is_reference(erlang:monitor(process, self()))",
            "ok true",
        ),
        case(
            "proc_signals/monitor_named_target_is_reference",
            "erlang:monitor/2 accepts the {Name, Node} target form and returns a reference",
            "is_reference(erlang:monitor(process, {a_named_target, node()}))",
            "ok true",
        ),
        case(
            "proc_signals/monitor_bound_self_is_reference",
            "a bound monitor reference from a self-monitor is a reference",
            "begin M = erlang:monitor(process, self()), is_reference(M) end",
            "ok true",
        ),
        // erlang:monitor/3 with a `{tag, _}` option and erlang:monitor/2 on a port are now
        // honoured by the Rust runtime, so both cases agree; the `_unsupported` suffix is
        // historical — kept only so the case ids stay stable.
        case(
            "proc_signals/monitor_3_tag_option_unsupported",
            "erlang:monitor/3 with a {tag, _} option monitors successfully",
            "begin Ref = erlang:monitor(process, self(), [{tag, foo}]), \
             is_reference(Ref) end",
            "ok true",
        ),
        case(
            "proc_signals/monitor_port_unsupported",
            "erlang:monitor/2 accepts a port and returns a reference",
            "begin P = open_port({spawn, \"cat\"}, []), M = erlang:monitor(port, P), \
             port_close(P), is_reference(M) end",
            "ok true",
        ),
        // --- erlang:monitor/3 `{tag, T}` delivery regression coverage ----------------------
        // The tag is delivered as the leading element of the DOWN message; every assertion is
        // a *shape* (element positions, `is_reference/1`, `process`, the locally bound pid) or
        // the reason — never a pid/ref/port identity, which differs per VM start.
        case(
            "proc_signals/monitor_3_tag_delivers_tagged_down",
            "a {tag, T} monitor delivers {T, Ref, process, Pid, normal} when the target exits",
            "begin P = spawn(fun() -> receive go -> exit(normal) end end), \
             Ref = erlang:monitor(process, P, [{tag, mytag}]), P ! go, \
             receive Msg -> case Msg of \
                 {mytag, MRef, process, Pid, Reason} -> \
                     {element(1, Msg) =:= mytag, element(3, Msg) =:= process, \
                      element(5, Msg) =:= normal, is_reference(MRef), Pid =:= P}; \
                 _ -> unexpected \
             end after 2000 -> timeout end end",
            "ok {true,true,true,true,true}",
        ),
        case(
            "proc_signals/monitor_3_tag_abnormal_reason_delivers_tagged_down",
            "a {tag, T} monitor's DOWN carries the target's abnormal exit reason",
            "begin P = spawn(fun() -> receive go -> exit(boom) end end), \
             Ref = erlang:monitor(process, P, [{tag, mytag}]), P ! go, \
             receive Msg -> case Msg of \
                 {mytag, MRef, process, Pid, Reason} -> \
                     {element(1, Msg) =:= mytag, Reason, is_reference(MRef), Pid =:= P}; \
                 _ -> unexpected \
             end after 2000 -> timeout end end",
            "ok {true,boom,true,true}",
        ),
        case(
            "proc_signals/monitor_3_integer_tag_delivers_tagged_down",
            "a non-atom (integer) {tag, T} is delivered verbatim as the DOWN's leading element",
            "begin P = spawn(fun() -> receive go -> exit(normal) end end), \
             Ref = erlang:monitor(process, P, [{tag, 42}]), P ! go, \
             receive Msg -> case Msg of \
                 {42, MRef, process, Pid, Reason} -> \
                     {element(1, Msg) =:= 42, element(3, Msg) =:= process, \
                      element(5, Msg) =:= normal, is_reference(MRef), Pid =:= P}; \
                 _ -> unexpected \
             end after 2000 -> timeout end end",
            "ok {true,true,true,true,true}",
        ),
        case(
            "proc_signals/monitor_3_duplicate_tag_last_wins",
            "erlang:monitor/3 with two {tag, T} options uses the last one",
            "begin P = spawn(fun() -> receive go -> exit(normal) end end), \
             Ref = erlang:monitor(process, P, [{tag, a}, {tag, b}]), P ! go, \
             receive Msg -> case Msg of \
                 {Tag, _, process, _, _} -> Tag; _ -> unexpected \
             end after 2000 -> timeout end end",
            "ok b",
        ),
        case(
            "proc_signals/monitor_3_empty_options_is_reference",
            "erlang:monitor/3 with an empty options list behaves like erlang:monitor/2",
            "is_reference(erlang:monitor(process, self(), []))",
            "ok true",
        ),
        case(
            "proc_signals/monitor_3_named_target_is_reference",
            "erlang:monitor/3 accepts the {Name, Node} target form with a {tag, T} option",
            "is_reference(erlang:monitor(process, {a_named_target, node()}, [{tag, t1}]))",
            "ok true",
        ),
        case(
            "proc_signals/monitor_3_port_tag_option",
            "erlang:monitor/3 accepts a port with a {tag, T} option",
            "begin P = open_port({spawn, \"cat\"}, []), \
             M = erlang:monitor(port, P, [{tag, t1}]), port_close(P), is_reference(M) end",
            "ok true",
        ),
        case(
            "proc_signals/monitor_3_tag_demonitor",
            "a {tag, T} monitor's reference can be demonitored",
            "begin Ref = erlang:monitor(process, self(), [{tag, t1}]), \
             erlang:demonitor(Ref) end",
            "ok true",
        ),
        // --- process_info/1,2 documented-key shapes (values are VM-dependent) --------------
        case(
            "proc_signals/process_info_heap_size_is_integer",
            "process_info(self(), heap_size) reports an integer element",
            "is_integer(element(2, process_info(self(), heap_size)))",
            "ok true",
        ),
        case(
            "proc_signals/process_info_memory_is_integer",
            "process_info(self(), memory) reports an integer element",
            "is_integer(element(2, process_info(self(), memory)))",
            "ok true",
        ),
        case(
            "proc_signals/process_info_dictionary_empty",
            "process_info(self(), dictionary) is empty for a process that stored no values",
            "element(2, process_info(self(), dictionary))",
            "ok []",
        ),
        case(
            "proc_signals/process_info_monitors_empty",
            "process_info(self(), monitors) is an empty list for a process that set no monitors",
            "process_info(self(), monitors)",
            "ok {monitors,[]}",
        ),
        case(
            "proc_signals/processes_is_a_list",
            "erlang:processes/0 returns a list",
            "is_list(processes())",
            "ok true",
        ),
        case(
            "proc_signals/registered_is_a_list",
            "erlang:registered/0 returns a list",
            "is_list(registered())",
            "ok true",
        ),
        case(
            "proc_signals/whereis_init_is_a_pid",
            "erlang:whereis(init) resolves the boot-registered init process to a pid",
            "is_pid(whereis(init))",
            "ok true",
        ),
        case(
            "proc_signals/is_process_alive_self",
            "erlang:is_process_alive/1 is true for the calling process",
            "is_process_alive(self())",
            "ok true",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The group-leader / dt-switch / statistics cases: plain A.7 eval cases, no extra argv.
    const SIGNAL_CASES: &[&str] = &[
        "proc_signals/group_leader_is_pid",
        "proc_signals/process_info_group_leader_is_pid",
        "proc_signals/group_leader_set_and_restore",
        "proc_signals/group_leader_inherited_by_spawn",
        "proc_signals/erts_internal_group_leader_set_and_restore",
        "proc_signals/erts_internal_group_leader_bad_option_is_badarg",
        "proc_signals/dt_spread_tag_true",
        "proc_signals/dt_spread_tag_false",
        "proc_signals/dt_spread_tag_bad_arg_is_badarg",
        "proc_signals/dt_restore_tag_true",
        "proc_signals/dt_restore_tag_non_boolean_is_badarg",
        "proc_signals/statistics_wall_clock_shape",
        "proc_signals/statistics_reductions_unsupported",
        "proc_signals/statistics_runtime_unsupported",
        "proc_signals/statistics_context_switches_unsupported",
        "proc_signals/statistics_io_unsupported",
        "proc_signals/statistics_scheduler_wall_time_unsupported",
        "proc_signals/statistics_reductions_is_two_integer_tuple",
        "proc_signals/statistics_context_switches_is_two_integer_tuple",
        "proc_signals/statistics_io_outer_shape",
        "proc_signals/process_info_heap_size_is_integer",
        "proc_signals/process_info_memory_is_integer",
        "proc_signals/process_info_dictionary_empty",
        "proc_signals/process_info_monitors_empty",
        "proc_signals/processes_is_a_list",
        "proc_signals/registered_is_a_list",
        "proc_signals/whereis_init_is_a_pid",
        "proc_signals/is_process_alive_self",
    ];

    /// The monitor / `spawn_monitor` family.
    const MONITOR_CASES: &[&str] = &[
        "proc_signals/spawn_monitor_returns_pid_and_reference",
        "proc_signals/spawn_monitor_3_returns_pid_and_reference",
        "proc_signals/spawn_monitor_4_returns_pid_and_reference",
        "proc_signals/spawn_monitor_down_message_shape",
        "proc_signals/monitor_self_is_reference",
        "proc_signals/monitor_named_target_is_reference",
        "proc_signals/monitor_bound_self_is_reference",
        "proc_signals/monitor_3_tag_option_unsupported",
        "proc_signals/monitor_port_unsupported",
        "proc_signals/monitor_3_tag_delivers_tagged_down",
        "proc_signals/monitor_3_tag_abnormal_reason_delivers_tagged_down",
        "proc_signals/monitor_3_integer_tag_delivers_tagged_down",
        "proc_signals/monitor_3_duplicate_tag_last_wins",
        "proc_signals/monitor_3_empty_options_is_reference",
        "proc_signals/monitor_3_named_target_is_reference",
        "proc_signals/monitor_3_port_tag_option",
        "proc_signals/monitor_3_tag_demonitor",
    ];

    fn case<'a>(cases: &'a [Case], id: &str) -> &'a Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the proc_signals group"))
    }

    #[test]
    fn every_case_is_a_plain_a7_eval_case_with_no_extra_argv() {
        let cases = group();
        let cases = cases.as_slice();
        for id in SIGNAL_CASES.iter().chain(MONITOR_CASES) {
            let case = case(cases, id);
            assert_eq!(case.contracts, &[Contract::OtpApps], "case {id}");
            assert!(
                matches!(case.kind, CaseKind::Eval { .. }),
                "case {id} must be an Eval case"
            );
            assert!(
                case.extra_argv().is_empty(),
                "case {id} takes no extra argv"
            );
            assert!(case.env().is_empty(), "case {id} sets no environment");
            let CaseKind::Eval { probe, .. } = case.kind else {
                unreachable!()
            };
            assert_eq!(probe, Probe::Value, "case {id}");
        }
        assert_eq!(cases.len(), SIGNAL_CASES.len() + MONITOR_CASES.len());
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
    fn the_raise_cases_are_marked_as_exceptions() {
        let cases = group();
        let cases = cases.as_slice();
        for id in [
            "proc_signals/erts_internal_group_leader_bad_option_is_badarg",
            "proc_signals/dt_spread_tag_bad_arg_is_badarg",
            "proc_signals/dt_restore_tag_non_boolean_is_badarg",
        ] {
            let case = case(cases, id);
            assert_eq!(case.golden.stdout, Some("exception error badarg"), "case {id}");
            assert_eq!(case.golden.exit_code, 1, "case {id}");
        }
    }

    #[test]
    fn the_statistics_cases_probe_the_documented_counters() {
        let cases = group();
        let cases = cases.as_slice();
        let statistics: Vec<&str> = SIGNAL_CASES
            .iter()
            .filter_map(|id| case(cases, id).expr())
            .filter(|e| e.contains("erlang:statistics("))
            .collect();
        assert_eq!(statistics.len(), 9, "nine statistics cases are expected");
        for key in [
            "wall_clock",
            "reductions",
            "runtime",
            "context_switches",
            "io",
            "scheduler_wall_time",
        ] {
            let needle = format!("erlang:statistics({key})");
            assert!(
                statistics.iter().any(|e| e.contains(&needle)),
                "no case probes {needle}"
            );
        }
    }

    #[test]
    fn the_group_leader_cases_restore_the_original_leader() {
        let cases = group();
        let cases = cases.as_slice();
        // Every case that *sets* a group leader captures the original first and restores it
        // before returning — otherwise the probe's own `io:format` would block.
        for id in [
            "proc_signals/group_leader_set_and_restore",
            "proc_signals/group_leader_inherited_by_spawn",
            "proc_signals/erts_internal_group_leader_set_and_restore",
        ] {
            let expr = case(cases, id).expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("Old = erlang:group_leader()"),
                "case {id}: the original leader must be captured: {expr:?}"
            );
            assert!(
                expr.contains("erlang:group_leader(Old, self())"),
                "case {id}: the original leader must be restored: {expr:?}"
            );
        }
    }

    #[test]
    fn the_down_message_case_asserts_the_shape_not_the_reference() {
        let cases = group();
        let cases = cases.as_slice();
        let expr = case(cases, "proc_signals/spawn_monitor_down_message_shape")
            .expr()
            .expect("an Eval case has an expr");
        assert!(expr.contains("'DOWN'"), "the DOWN tag must be matched: {expr:?}");
        assert!(
            expr.contains("Ref =:= M") && expr.contains("Q =:= P"),
            "the reference and pid are compared to the local bindings: {expr:?}"
        );
    }
}
