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
//! A.1/A.6 — the `-eval` route's own semantics: a send whose destination is already gone, and the
//! deadline of a `receive … after T`.
//!
//! Expression cases: both runtimes get the **same** `-eval` argv, so the probe's single output line
//! is the whole observable. Every case therefore declares **A.6** (the `-eval` CLI route is what is
//! being driven — a route that mis-evaluates the script is what these cases detect) alongside
//! **A.1** (the language semantics that route has to reproduce).
//!
//! # A send to an already-dead pid is a silent success
//!
//! `Pid ! Msg` where `Pid` names no live process must return `Msg` and must **not** raise: the C's
//! `do_send` looks the pid up and reports success without delivering anything. The Rust `-eval`
//! route raised `TermNotFound` here (`beam_rt: term not found`, exit 1), and — the "fold asymmetry"
//! — only in some positions: `!` worked *before* a blocking call and failed *after* one. So **every**
//! dead-pid case sends *after* a blocking call, in three different shapes: a `timer:sleep/1`, a
//! bounded `receive … after`, and a `receive` that waited for the destination's own `'DOWN'`
//! message (which proves the destination is gone). A fix that special-cased only the
//! non-blocking path would fail them. The returned value is the message itself (`Msg`, never
//! `true`), which is what makes the whole group a single-line pin.
//!
//! A **name** is the contrary case and is pinned as such: `Name ! Msg` for a name that is not
//! registered — never was, was `unregister/1`ed, or died with its own process — raises
//! `error:badarg` on both runtimes. Only a *pid* destination is silent, so a fix that folded every
//! destination-shaped lookup failure would break these four cases.
//!
//! The dead-pid and dead-name expressions are never wrapped in `catch`: the payload of a caught
//! raise carries the raise-site stacktrace, and a stacktrace is a listed unpinnable shape (the
//! reason degrades, `divergences.md` §A.1/A.7), so `catch` here would manufacture a `mismatch`
//! from an unrelated, documented difference.
//!
//! # A `receive … after T` ends on its own deadline
//!
//! The route used to pump the scheduler in ~150 ms blocking slices and re-check the mailbox and the
//! deadline only *between* whole pumps, so every `after T` overshot by up to one slice — a flat
//! ~150 ms extra for `after 0` … `after 100`. Two cases pin the two halves of that with
//! **one-sided** bounds, because a two-sided bucket on a wall-clock duration is only as reliable as
//! the host's load:
//!
//! * `receive_after_rounds_add_no_fixed_delay` — eight consecutive `after 0` rounds must finish
//!   inside `300` ms. There is no legitimate reason for an **immediate** deadline to wait at all,
//!   so the bound sits ~300× above the truth and ~4× below the defective route's ≈1.2 s.
//! * `receive_after_rounds_wait_their_deadline` — twelve consecutive `after 20` rounds must take at
//!   least `150` ms, i.e. the deadline must really be waited for (a route that answered the `after`
//!   body immediately would be wrong in the other direction). Being a *lower* bound, load can only
//!   ever help it pass.
//!
//! Measured on an idle host (median of 5 runs each; OTP 29.1.1 / erts-17.1 oracle against a release
//! `beam_rt`): eight `after 0` rounds cost **oracle 0 ms / rust 0 ms**, and twelve `after 20`
//! rounds cost **oracle 252–254 ms / rust 241–245 ms**. The old route's figures were ≈150 ms *per*
//! `after`, i.e. ≈1.2 s and ≈2.0 s for those two shapes. The elapsed value itself is deliberately
//! **not** part of any asserted golden — only the two booleans are.
//!
//! # Load note
//!
//! An extreme overload (28 spinning processes on 14 cores) inflates *both* runtimes' wall clock
//! without bound — the oracle's own twelve `after 20` rounds reached 2.8–15.1 s — which is why the
//! `after 20` case asserts a lower bound only. It also surfaces the documented parallel-load
//! artifact of the CLI route (`beam_rt: unsupported: opcode: run: process blocked (no message
//! available)`, classified `unsupported`, never `mismatch`); a serial re-run agrees.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One `-eval`-route case: evaluate `expr` under both runtimes and pin the oracle's `~w` rendering.
fn case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::LanguageSemantics, Contract::Cli],
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

/// One case whose expression raises: the golden is the probe's `exception …` line and the exit
/// code is the probe's `1`.
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

/// The A.1/A.6 `-eval`-route cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- a send to an already-dead pid: silent success, the message is the value ----------
        case(
            "eval_route/send_to_a_dead_pid_returns_the_message",
            "`Pid ! Msg` to a pid that has exited returns Msg and does not raise",
            "begin Pid = spawn(fun() -> ok end), receive after 50 -> ok end, Pid ! hello end",
            "ok hello",
        ),
        case(
            "eval_route/send_to_a_dead_pid_after_a_sleep_returns_the_message",
            "the dead-pid `!` still returns the message after a blocking timer:sleep/1 (the \
             route's `!`-after-a-blocking-call defect)",
            "begin Pid = spawn(fun() -> ok end), timer:sleep(50), Pid ! hello end",
            "ok hello",
        ),
        case(
            "eval_route/send_to_a_dead_pid_after_a_blocking_receive_returns_the_message",
            "the dead-pid `!` still returns the message after a receive that waited for the \
             destination's own 'DOWN' message (the destination is provably gone)",
            "begin P = spawn(fun() -> ok end), Ref = erlang:monitor(process, P), \
             receive {'DOWN', Ref, process, P, Why} -> ok end, P ! hello end",
            "ok hello",
        ),
        case(
            "eval_route/send_to_a_dead_pid_twice_returns_the_second_message",
            "two sends to the same dead pid both succeed and report their own message",
            "begin P = spawn(fun() -> ok end), Ref = erlang:monitor(process, P), \
             receive {'DOWN', Ref, process, P, Why} -> ok end, {P ! a, P ! b} end",
            "ok {a,b}",
        ),
        case(
            "eval_route/send_to_a_dead_pid_returns_a_tuple_payload",
            "the value of a send to a dead pid is the tuple payload itself",
            "begin P = spawn(fun() -> ok end), timer:sleep(50), P ! {a,1} end",
            "ok {a,1}",
        ),
        case(
            "eval_route/send_to_a_dead_pid_returns_a_binary_payload",
            "the value of a send to a dead pid is the binary payload itself",
            "begin P = spawn(fun() -> ok end), timer:sleep(50), P ! <<1,2,3>> end",
            "ok <<1,2,3>>",
        ),
        case(
            "eval_route/send_to_a_dead_pid_in_a_value_position",
            "a dead-pid send nested in a term evaluates to the message, not to an error",
            "begin P = spawn(fun() -> ok end), timer:sleep(50), self() ! {sent, P ! hello}, \
             receive Z -> Z after 2000 -> timeout end end",
            "ok {sent,hello}",
        ),
        case(
            "eval_route/erlang_send_to_a_dead_pid_returns_the_message",
            "erlang:send/2 to a dead pid returns the message (the same fold as `!`)",
            "begin P = spawn(fun() -> ok end), timer:sleep(50), erlang:send(P, hello) end",
            "ok hello",
        ),
        // --- contrast: a live destination delivers and still returns the message --------------
        case(
            "eval_route/send_to_a_live_pid_returns_the_message",
            "`Pid ! Msg` to a live pid returns Msg (no delivery is observable here, only the value)",
            "begin Pid = spawn(fun() -> receive M -> M end end), Pid ! hello end",
            "ok hello",
        ),
        case(
            "eval_route/erlang_send_to_a_live_pid_returns_the_message",
            "erlang:send/2 to a live pid returns the message",
            "begin Pid = spawn(fun() -> receive M -> M end end), erlang:send(Pid, direct) end",
            "ok direct",
        ),
        case(
            "eval_route/send_to_a_live_registered_name_returns_the_message",
            "`Name ! Msg` to a registered live process returns Msg",
            "begin register(alive_name, spawn(fun() -> receive M -> M end end)), \
             alive_name ! hi end",
            "ok hi",
        ),
        // --- the contrary case: an unregistered NAME destination is badarg -------------------
        raises(
            "eval_route/send_to_an_unregistered_name_is_badarg",
            "`Name ! Msg` for a name that was never registered raises error:badarg (a name is not \
             a dead pid)",
            "nosuchname_here ! hello",
            "exception error badarg",
        ),
        raises(
            "eval_route/send_to_a_name_unregistered_before_the_send_is_badarg",
            "`Name ! Msg` after unregister/1 raises error:badarg",
            "begin P = spawn(fun() -> receive go -> ok end end), register(doomed, P), \
             unregister(doomed), doomed ! hello end",
            "exception error badarg",
        ),
        raises(
            "eval_route/send_to_a_name_that_died_with_its_process_is_badarg",
            "`Name ! Msg` after the registered process exited raises error:badarg",
            "begin P = spawn(fun() -> ok end), register(doomed2, P), \
             Ref = erlang:monitor(process, P), \
             receive {'DOWN', Ref, process, P, Why} -> ok end, doomed2 ! hello end",
            "exception error badarg",
        ),
        raises(
            "eval_route/erlang_send_to_an_unregistered_name_is_badarg",
            "erlang:send/2 to an unregistered name raises error:badarg",
            "erlang:send({nosuchname2}, hello)",
            "exception error badarg",
        ),
        // --- receive … after T: the deadline family ----------------------------------------
        case(
            "eval_route/receive_after_with_no_message_answers_its_body",
            "a receive with nothing to match runs its `after` body",
            "begin receive Y -> Y after 20 -> timeout end end",
            "ok timeout",
        ),
        case(
            "eval_route/receive_after_skips_a_non_matching_message",
            "a selective receive whose mailbox holds only a non-matching message runs its \
             `after` body",
            "begin self() ! other, receive wanted -> wanted after 20 -> timeout end end",
            "ok timeout",
        ),
        case(
            "eval_route/receive_finds_a_message_already_in_the_mailbox",
            "a message that is already in the mailbox is taken immediately, without waiting for \
             the deadline",
            "begin self() ! pre, Got = receive Y -> Y after 2000 -> timeout end, Got end",
            "ok pre",
        ),
        case(
            "eval_route/receive_picks_up_a_message_that_beats_the_deadline",
            "a message that arrives before the deadline is delivered instead of the `after` body",
            "begin Parent = self(), \
             _ = spawn(fun() -> timer:sleep(10), Parent ! x end), \
             Got = receive Y -> Y after 2000 -> timeout end, Got end",
            "ok x",
        ),
        case(
            "eval_route/send_after_is_delivered_to_its_own_receive",
            "erlang:send_after/3's message reaches the receive that waits for it",
            "begin erlang:send_after(20, self(), tick), \
             receive T -> T after 2000 -> timeout end end",
            "ok tick",
        ),
        // The two timing cases. Neither pins a duration: each answers a count (that *all* the
        // rounds ran) plus a single boolean derived from the elapsed monotonic time.
        case(
            "eval_route/receive_after_rounds_add_no_fixed_delay",
            "eight consecutive `receive … after 0` rounds finish inside 300 ms (an immediate \
             deadline must not wait for a scheduler pump slice)",
            "begin T0 = erlang:monotonic_time(millisecond), \
             G = fun Loop(0) -> ok; \
             Loop(N) -> receive never -> never after 0 -> Loop(N-1) end end, \
             Done = G(8), El = erlang:monotonic_time(millisecond) - T0, {Done, El < 300} end",
            "ok {ok,true}",
        ),
        case(
            "eval_route/receive_after_rounds_wait_their_deadline",
            "twelve consecutive `receive … after 20` rounds all run and take at least 150 ms (the \
             deadline is really waited for)",
            "begin T0 = erlang:monotonic_time(millisecond), \
             G = fun Loop(0) -> ok; \
             Loop(N) -> receive never -> never after 20 -> Loop(N-1) end end, \
             Done = G(12), El = erlang:monotonic_time(millisecond) - T0, {Done, El >= 150} end",
            "ok {ok,true}",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ids of the cases whose destination is a pid that is already dead (`!` and `erlang:send/2`).
    const DEAD_PID_CASES: &[&str] = &[
        "eval_route/send_to_a_dead_pid_returns_the_message",
        "eval_route/send_to_a_dead_pid_after_a_sleep_returns_the_message",
        "eval_route/send_to_a_dead_pid_after_a_blocking_receive_returns_the_message",
        "eval_route/send_to_a_dead_pid_twice_returns_the_second_message",
        "eval_route/send_to_a_dead_pid_returns_a_tuple_payload",
        "eval_route/send_to_a_dead_pid_returns_a_binary_payload",
        "eval_route/send_to_a_dead_pid_in_a_value_position",
        "eval_route/erlang_send_to_a_dead_pid_returns_the_message",
    ];

    /// The ids of the cases whose destination is a **live** pid or a live registered name.
    const LIVE_DESTINATION_CASES: &[&str] = &[
        "eval_route/send_to_a_live_pid_returns_the_message",
        "eval_route/erlang_send_to_a_live_pid_returns_the_message",
        "eval_route/send_to_a_live_registered_name_returns_the_message",
    ];

    /// The ids of the cases whose destination is a name that is not registered (the contrary case:
    /// `badarg`, never a silent success).
    const UNREGISTERED_NAME_CASES: &[&str] = &[
        "eval_route/send_to_an_unregistered_name_is_badarg",
        "eval_route/send_to_a_name_unregistered_before_the_send_is_badarg",
        "eval_route/send_to_a_name_that_died_with_its_process_is_badarg",
        "eval_route/erlang_send_to_an_unregistered_name_is_badarg",
    ];

    /// The ids of the `receive … after T` cases (the deadline family).
    const RECEIVE_AFTER_CASES: &[&str] = &[
        "eval_route/receive_after_with_no_message_answers_its_body",
        "eval_route/receive_after_skips_a_non_matching_message",
        "eval_route/receive_finds_a_message_already_in_the_mailbox",
        "eval_route/receive_picks_up_a_message_that_beats_the_deadline",
        "eval_route/send_after_is_delivered_to_its_own_receive",
        "eval_route/receive_after_rounds_add_no_fixed_delay",
        "eval_route/receive_after_rounds_wait_their_deadline",
    ];

    /// The two timing cases, which are the only ones allowed to mention the monotonic clock.
    const TIMING_CASES: &[&str] = &[
        "eval_route/receive_after_rounds_add_no_fixed_delay",
        "eval_route/receive_after_rounds_wait_their_deadline",
    ];

    fn case<'a>(cases: &'a [Case], id: &str) -> &'a Case {
        cases
            .iter()
            .find(|case| case.id == id)
            .unwrap_or_else(|| panic!("no case with the id {id}"))
    }

    #[test]
    fn the_group_is_the_expected_size_and_every_id_is_namespaced() {
        let cases = group();
        assert_eq!(
            cases.len(),
            DEAD_PID_CASES.len()
                + LIVE_DESTINATION_CASES.len()
                + UNREGISTERED_NAME_CASES.len()
                + RECEIVE_AFTER_CASES.len(),
            "the group size is pinned: add the id to its family constant with the case"
        );
        for case in &cases {
            assert!(
                case.id.starts_with("eval_route/"),
                "case {}: the id must be namespaced under eval_route/",
                case.id
            );
        }
        // Every id in the family constants is a real case (and the four families are disjoint, so
        // a case cannot be counted twice).
        let expected: Vec<&str> = DEAD_PID_CASES
            .iter()
            .chain(LIVE_DESTINATION_CASES)
            .chain(UNREGISTERED_NAME_CASES)
            .chain(RECEIVE_AFTER_CASES)
            .copied()
            .collect();
        for id in &expected {
            case(&cases, id);
        }
        let mut sorted = expected.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), expected.len(), "the family constants overlap");
    }

    #[test]
    fn every_case_is_an_eval_case_with_a_value_probe_and_no_extra_argv() {
        for case in group() {
            assert_eq!(
                case.contracts,
                &[Contract::LanguageSemantics, Contract::Cli],
                "case {}: a `-eval`-route case covers A.1 through the A.6 route",
                case.id
            );
            let CaseKind::Eval {
                probe, extra_argv, ..
            } = case.kind
            else {
                panic!("case {}: must be an Eval case", case.id);
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
            assert!(
                extra_argv.is_empty(),
                "case {}: no extra argv — the route itself, not a flag, is under test",
                case.id
            );
            assert!(
                case.env().is_empty(),
                "case {}: a `-eval`-route case carries no spawn environment",
                case.id
            );
        }
    }

    /// The `badarg` family is the only one that raises, and it raises exactly `error:badarg` with
    /// the probe's exit code `1`; every other case answers a value and exits `0`.
    #[test]
    fn only_the_name_cases_raise_and_they_raise_badarg() {
        let cases = group();
        let cases = cases.as_slice();
        for case in cases {
            let golden = case.golden.stdout.expect("every case asserts stdout");
            let is_name_case = UNREGISTERED_NAME_CASES.contains(&case.id);
            if is_name_case {
                assert_eq!(golden, "exception error badarg", "case {}", case.id);
                assert_eq!(case.golden.exit_code, 1, "case {}", case.id);
            } else {
                assert!(
                    golden.starts_with("ok "),
                    "case {}: a non-raising case's golden is an `ok …` line: {golden:?}",
                    case.id
                );
                assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            }
        }
    }

    /// A golden may not pin a volatile value: a per-VM-start pid/reference, a branch-specific
    /// reason, or a raw wall-clock duration.
    #[test]
    fn goldens_never_pin_a_volatile_value() {
        for case in group() {
            let golden = case.golden.stdout.expect("every case asserts stdout");
            for needle in ["<0.", "#Ref<"] {
                assert!(
                    !golden.contains(needle),
                    "case {}: the golden pins an unstable identity: {golden:?}",
                    case.id
                );
            }
            // No golden is a bare duration (`ok 242`): the timing cases answer booleans only.
            let bare_duration = golden
                .strip_prefix("ok ")
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()));
            assert!(
                !bare_duration,
                "case {}: the golden pins a raw duration: {golden:?}",
                case.id
            );
        }
    }

    /// A probe expression must not bind the wrapper's own `V`, `C` or `R`: a collision makes the
    /// *oracle* reject the wrapped probe, i.e. a guaranteed false mismatch.
    #[test]
    fn no_expression_binds_a_probe_wrapper_variable() {
        for case in group() {
            let expr = case.expr().expect("an Eval case has an expr");
            for reserved in [" V ", " C ", " R ", "V =", "C =", "R =", "V)", "C)", "R)"] {
                assert!(
                    !expr.contains(reserved),
                    "case {}: the expression binds the probe variable {reserved:?}: {expr:?}",
                    case.id
                );
            }
        }
    }

    /// No expression is wrapped in `catch`: a caught raise carries the raise-site stacktrace, whose
    /// payload is a documented unpinnable shape, so `catch` would turn an unrelated difference into
    /// a mismatch. (The raising family raises *uncaught* — the probe reports the class and reason.)
    #[test]
    fn no_expression_catches_a_raise() {
        for case in group() {
            let expr = case.expr().expect("an Eval case has an expr");
            assert!(
                !expr.contains("catch "),
                "case {}: a caught raise would pin the raise-site stacktrace: {expr:?}",
                case.id
            );
        }
    }

    /// Every dead-pid case sends to a pid that is provably (or overwhelmingly likely to be) gone,
    /// and — the point of the group — **after** a blocking call, the exact position the route used
    /// to fail in. All three blocking shapes occur.
    #[test]
    fn every_dead_pid_case_sends_after_a_blocking_call() {
        let cases = group();
        let cases = cases.as_slice();
        let mut shapes = [false; 3];
        for id in DEAD_PID_CASES {
            let expr = case(cases, id).expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("spawn("),
                "case {id}: the destination must be a spawned pid: {expr:?}"
            );
            assert!(
                expr.contains(" ! ") || expr.contains("erlang:send("),
                "case {id}: the expression must send: {expr:?}"
            );
            let blocking = ["timer:sleep(", "receive after ", "'DOWN'"];
            let mut found = false;
            for (index, needle) in blocking.iter().enumerate() {
                if expr.contains(needle) {
                    shapes[index] = true;
                    found = true;
                }
            }
            assert!(
                found,
                "case {id}: the send must follow a blocking call — that is the position the \
                 route used to fail in: {expr:?}"
            );
        }
        assert!(
            shapes.iter().all(|seen| *seen),
            "all three blocking shapes must occur ({blocking:?}): {shapes:?}",
            blocking = ["timer:sleep/1", "receive … after", "'DOWN'"]
        );
    }

    /// The timing cases classify the elapsed monotonic time into **one-sided** bounds and never put
    /// the raw value in the answer.
    #[test]
    fn the_timing_cases_pin_one_sided_bounds_not_a_duration() {
        let cases = group();
        let cases = cases.as_slice();
        let no_delay = case(&cases, "eval_route/receive_after_rounds_add_no_fixed_delay")
            .expr()
            .expect("an Eval case has an expr");
        assert!(
            no_delay.contains("erlang:monotonic_time(millisecond)")
                && no_delay.contains("after 0")
                && no_delay.contains("{Done, El < 300}"),
            "the fixed-delay case must bound the elapsed time one-sidedly: {no_delay:?}"
        );
        let deadline = case(
            &cases,
            "eval_route/receive_after_rounds_wait_their_deadline",
        )
        .expr()
        .expect("an Eval case has an expr");
        assert!(
            deadline.contains("after 20") && deadline.contains("{Done, El >= 150}"),
            "the deadline case must assert a lower bound only: {deadline:?}"
        );
        // The elapsed value never reaches the answer, in either case.
        for expr in [no_delay, deadline] {
            assert!(
                !expr.contains(", El}") && !expr.contains("{Done, El,"),
                "the raw elapsed time must not be part of the pinned answer: {expr:?}"
            );
        }
        // ... and no *other* case mentions the clock at all.
        for case in cases {
            if TIMING_CASES.contains(&case.id) {
                continue;
            }
            let expr = case.expr().expect("an Eval case has an expr");
            assert!(
                !expr.contains("monotonic_time") && !expr.contains("statistics("),
                "case {}: only the timing cases may read a clock: {expr:?}",
                case.id
            );
        }
    }
}
