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

//! Contract **A.1** — `erlang:monitor/3`'s `{tag, Tag}` **user tag** and the `demonitor/2`
//! `[flush]` interaction with it.
//!
//! A tagged monitor delivers `{Tag, Ref, Kind, Target, Reason}` — the ordinary DOWN with the
//! caller's term in the **head** slot (`monitor/3` itself is pinned by [`super::proc_signals`],
//! which asserts the shape of the notification). This group pins the *storage and identity* half
//! of that feature, which is the part a runtime can silently get wrong:
//!
//! * **the tag is copied at monitor-creation time.** The C keeps an immediate (or a frozen
//!   literal-area term) inline in the monitor allocation and copies every other shape into its
//!   own `ErlHeapFragment` (`mk_heap_frag_eterm`,
//!   `erts/emulator/beam/erl_monitor_link.c:833-900`), so a tag resident on the watcher's own
//!   heap is **not** a heap word of the watcher's after that point. The observable consequence,
//!   and what [`monitor_tag/tagged_down_with_a_heap_resident_tag_echoes_the_tag`] pins, is that
//!   the tag still arrives `=:=` the original after the watcher has **collected its own heap**
//!   while the monitor was outstanding: a tag stored as a raw heap word would dangle there.
//! * **a big binary tag survives the copy with its payload intact**
//!   ([`monitor_tag/a_binary_tag_survives_the_copy_with_its_size`]): a 1000-byte binary is far
//!   above `ERL_ONHEAP_BINARY_LIMIT`, so its bytes are a real refcounted object that the slot's
//!   copy has to re-register, and `byte_size/1` of the delivered head is the oracle's `1000`.
//! * **the flush is head-agnostic** ([`monitor_tag/demonitor_flush_removes_a_queued_tagged_down`]):
//!   `demonitor(Ref, [flush])` traps to `erts_internal:flush_monitor_messages/3`, whose pattern
//!   is `{_, Ref, _, _, _}` — a head test on `'DOWN'` would leak a tagged DOWN into the caller's
//!   mailbox (the shape `gen_server`'s `mc_recv/5` leaves behind). So a queued *tagged* DOWN is
//!   removed, the mailbox is empty afterwards and `demonitor/2` still answers `true`.
//! * **an immediate tag is stored verbatim and echoed unchanged**
//!   ([`monitor_tag/an_immediate_tag_is_stored_and_echoed_verbatim`]).
//!
//! # Golden discipline
//!
//! Every golden is the oracle's own line, measured with the probe wrapper below — never
//! hand-written — and it is order-stable: booleans, integer sizes and atoms only. No pid, port,
//! reference or creation value is printed (a monitor reference is compared with `=:=` inside the
//! expression, never rendered), and the reason slot is the atom the target exits with.
//! `element(1, M) =:= T` compares the delivered tag against a binding made **in the same
//! expression**, which is the identity assertion; the oracle prints `true` for it.
//!
//! # Waits
//!
//! Every wait on the probe process itself is bounded (`receive … after 2000 -> timeout end`), and
//! no case sleeps. The `receive go -> … end` inside a spawned helper is unbounded by design and
//! needs no timeout: the send that unblocks it is the next statement of the case, so the helper
//! always terminates (the same shape the sibling [`super::proc_signals`] and
//! [`super::proc_control`] rows use).
//!
//! # The flush row re-queues the DOWN instead of sleeping
//!
//! `demonitor(Ref, [flush])` must be measured on a DOWN that is **already in the mailbox**, but
//! receiving it would remove it — and the oracle delivers a monitor DOWN asynchronously (a
//! `monitor/2` on an already-dead pid answers `message_queue_len` `0` right after the call). So
//! the row receives the DOWN as its synchronisation device and immediately sends it back to
//! itself (`self() ! M`); the flush's pattern is identity-based, so the re-queued copy is the
//! one it removes. That is why the row reads `{1, true, 0}` and why it needs no `timer:sleep`.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One `monitor_tag/*` eval case: evaluate `expr` under both runtimes, pin the oracle's `~w`
/// rendering of its answer.
///
/// [`Probe::Value`] prints the expression's own answer as one readable line; every case here
/// answers a small tuple of booleans/atoms/an integer, so the line is order-stable. Every case
/// exits `0` — nothing is left to raise.
fn case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::LanguageSemantics],
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

/// The A.1 `monitor/3` user-tag group: `monitor_tag/*`.
pub(super) fn group() -> Vec<Case> {
    vec![
        // --- the heap-resident tag: copied at creation, echoed after the watcher collected ----
        case(
            "monitor_tag/tagged_down_with_a_heap_resident_tag_echoes_the_tag",
            "a heap-resident {tag, T} (a list carrying a reference and a map) is copied at \
             monitor creation, so the DOWN still carries a tag =:= T after the watcher collected \
             its own heap twice, and the reason is the target's `boom`",
            "begin T = [a, make_ref(), #{k => <<\"v\">>}], \
             P = spawn(fun() -> receive go -> exit(boom) end end), \
             _ = erlang:monitor(process, P, [{tag, T}]), \
             Junk = lists:duplicate(50, lists:seq(1, 50)), \
             erlang:garbage_collect(), erlang:garbage_collect(), \
             P ! go, \
             M = receive X -> X after 2000 -> timeout end, \
             {element(1, M) =:= T, is_reference(element(2, M)), \
              element(3, M) =:= process, is_pid(element(4, M)), element(5, M)} end",
            "ok {true,true,true,true,boom}",
        ),
        // --- a big binary tag: an off-heap payload the slot's copy re-registers ---------------
        case(
            "monitor_tag/a_binary_tag_survives_the_copy_with_its_size",
            "a 1000-byte binary {tag, T} (above ERL_ONHEAP_BINARY_LIMIT, so a refcounted \
             off-heap payload) arrives =:= T with byte_size/1 = 1000 and the target's `bye`",
            "begin T = binary:copy(<<\"x\">>, 1000), \
             P = spawn(fun() -> receive go -> exit(bye) end end), \
             _ = erlang:monitor(process, P, [{tag, T}]), \
             P ! go, \
             M = receive X -> X after 2000 -> timeout end, \
             {element(1, M) =:= T, byte_size(element(1, M)), element(5, M)} end",
            "ok {true,1000,bye}",
        ),
        // --- the flush: head-agnostic, so a queued TAGGED DOWN goes too -----------------------
        case(
            "monitor_tag/demonitor_flush_removes_a_queued_tagged_down",
            "demonitor(Ref, [flush]) removes an already-queued tagged DOWN (its pattern is \
             {_, Ref, _, _, _}, not a 'DOWN' head test): the mailbox is 1 before and 0 after, and \
             the call answers true",
            "begin Q = fun() -> element(2, erlang:process_info(self(), message_queue_len)) end, \
             P = spawn(fun() -> receive go -> exit(normal) end end), \
             Ref = erlang:monitor(process, P, [{tag, mytag}]), \
             P ! go, \
             M = receive X -> X after 2000 -> timeout end, \
             self() ! M, \
             B = Q(), D = erlang:demonitor(Ref, [flush]), A = Q(), \
             {B, D, A} end",
            "ok {1,true,0}",
        ),
        // --- an immediate tag: stored verbatim ------------------------------------------------
        case(
            "monitor_tag/an_immediate_tag_is_stored_and_echoed_verbatim",
            "an immediate (atom) {tag, immed} is stored verbatim and delivered unchanged, with \
             the target's own `immed` as the reason",
            "begin P = spawn(fun() -> receive go -> exit(immed) end end), \
             _ = erlang:monitor(process, P, [{tag, immed}]), \
             P ! go, \
             M = receive X -> X after 2000 -> timeout end, \
             {element(1, M) =:= immed, element(5, M)} end",
            "ok {true,immed}",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, CaseKind, Contract, Probe};

    /// The group is a real A.1 group: ids are `monitor_tag/`-prefixed, unique and described, and
    /// every case is a self-terminating `eval` case whose golden is one non-empty line at exit
    /// `0`.
    #[test]
    fn every_case_is_a_described_exit_zero_monitor_tag_eval_case() {
        let cases = group();
        assert_eq!(cases.len(), 4, "the group's size is pinned by this test");
        let mut ids = Vec::new();
        for case in &cases {
            assert!(
                case.id.starts_with("monitor_tag/"),
                "case {} is not in the monitor_tag/ group",
                case.id
            );
            assert!(
                case.contracts.contains(&Contract::LanguageSemantics),
                "case {} must cover contract A.1",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {} must exit 0", case.id);
            let golden = case
                .golden
                .stdout
                .expect("every case carries a golden line");
            assert!(!golden.is_empty(), "case {} has an empty golden", case.id);
            assert!(
                !golden.contains('\n'),
                "case {} pins more than one line",
                case.id
            );
            assert!(
                golden.starts_with("ok "),
                "case {}: unexpected golden shape {golden:?}",
                case.id
            );
            match case.kind {
                CaseKind::Eval {
                    extra_argv, probe, ..
                } => {
                    assert!(
                        extra_argv.is_empty(),
                        "case {} must not carry extra argv",
                        case.id
                    );
                    assert!(
                        matches!(probe, Probe::Value),
                        "case {} must pin the readable ~w line",
                        case.id
                    );
                }
                _ => panic!("case {} is not an eval case", case.id),
            }
            ids.push(case.id);
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "duplicate case id in the monitor_tag/ group"
        );
    }

    /// Every row really is a `{tag, T}` monitor case: it monitors a process through
    /// `erlang:monitor/3` with a tag option, waits (boundedly) for the notification and then
    /// observes it — either the delivered head (the tag, through `element(1, M)`) or, for the
    /// flush row, the re-queued message the flush is measured on.
    #[test]
    fn every_case_tags_a_process_monitor() {
        for case in group() {
            let expr = case.expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("erlang:monitor(process, ") && expr.contains("[{tag, "),
                "case {} never tags a process monitor",
                case.id
            );
            assert!(
                expr.contains("M = receive X -> X after 2000 -> timeout end"),
                "case {} never waits for the notification",
                case.id
            );
            assert!(
                expr.contains("element(1, M)") || expr.contains("self() ! M"),
                "case {} never observes the notification",
                case.id
            );
        }
    }

    /// The flush row is the only one that calls `demonitor/2`, and it does so with `[flush]` on a
    /// DOWN it re-queued itself (`self() ! M`) — the mailbox reading is only meaningful then.
    #[test]
    fn the_flush_row_requeues_its_down_before_flushing() {
        let cases = group();
        let mut flushes = 0;
        for case in &cases {
            let expr = case.expr().expect("an Eval case has an expr");
            let calls = expr.matches("erlang:demonitor(").count();
            if calls == 0 {
                continue;
            }
            flushes += 1;
            assert_eq!(
                case.id, "monitor_tag/demonitor_flush_removes_a_queued_tagged_down",
                "case {} must be the group's only demonitor row",
                case.id
            );
            assert!(
                expr.contains("erlang:demonitor(Ref, [flush])"),
                "case {} must flush",
                case.id
            );
            assert!(
                expr.contains("self() ! M"),
                "case {} must re-queue the DOWN it received, so the flush has something to remove",
                case.id
            );
        }
        assert_eq!(flushes, 1, "exactly one row pins the flush");
    }

    /// The bounded-wait discipline: no case sleeps, every wait on the probe process itself
    /// carries an `after` branch, and the spawned helper's `receive go` is unblocked by the case's
    /// own next statement (so it can never hang).
    #[test]
    fn no_case_sleeps_and_every_wait_is_bounded() {
        for case in group() {
            let expr = case.expr().expect("an Eval case has an expr");
            assert!(
                !expr.contains("timer:sleep"),
                "case {} synchronises with a sleep",
                case.id
            );
            assert!(
                expr.contains("after 2000 -> timeout end"),
                "case {} has no bounded wait",
                case.id
            );
            assert!(
                expr.contains("P ! go, "),
                "case {} never unblocks its spawned helper",
                case.id
            );
        }
    }

    /// No golden may pin a volatile identity: a pid or a reference rendering differs per VM start.
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
}
