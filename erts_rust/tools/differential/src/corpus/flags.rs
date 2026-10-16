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

//! Contract **A.6** — CLI flags and configuration.
//!
//! `init:get_argument/1` is the observable side of the emulator command line: a runtime
//! that accepts the same argv must expose the same arguments, including user flags it
//! does not know about (`-differential_test_flag hello` here). These cases drive both
//! runtimes with the *same* `["-noshell", <extra…>, "-eval", <probe>]` argv (see
//! [`CaseKind::Eval`](super::CaseKind::Eval)) and pin how the parser groups occurrences:
//! a flag appears one inner list per occurrence, in argv order, each occurrence carrying
//! the following non-switch tokens as its values (so `-pa`
//! [code path](https://www.erlang.org/doc/man/erl.html) is two occurrences and ordering
//! is preserved), a bare flag is the valueless occurrence `[]`, a recognized switch
//! (`-sname`, `-name`, `-setcookie`) and an unknown `-flag` are recorded identically, and
//! a flag that was never passed is reported as the atom `error`.
//!
//! Only flag forms both runtimes parse to the *same* `init:get_argument/1` answer are
//! pinned here; forms that diverge (e.g. a plain argument after `-extra`/`--`, which the
//! oracle reports through `init:get_plain_arguments/0` while this runtime drops it) are
//! documented handoffs instead of cases.
//!
//! The `flags/env_*` cases drive the *environment half* of the same contract through
//! [`CaseKind::EvalEnv`](super::CaseKind::EvalEnv): `erl` merges
//! `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS] [ERL_ZFLAGS]` into the
//! argv before `init` sees it, so both runtimes are spawned with the same argv **and** the
//! same environment and the merged switches are observed through the `-eval` probe just
//! like command-line ones. `ERL_OTP<sysver>_FLAGS` is unpinnable (see the crate CONTEXT
//! *Known Issues*). `ERL_MAX_PORTS` is the one non-`*FLAGS` variable pinned here: it
//! requests a port-table size through the same channel as `+Q N`, observed through
//! `erlang:system_info(port_limit)`.
//!
//! The `flags/plus_q_*` cases pin the `+Q N` → `port_limit` sizing (round up to the next
//! power of two, capped at the hard ceiling), and `flags/erl_max_ports_sets_port_limit`
//! pins the `ERL_MAX_PORTS` request through the same observable.
//!
//! The `flags/plus_hmax*`/`flags/plus_hms*`/`flags/plus_hmbs*` cases pin the emulator's
//! `max_heap_size` / minimum-heap-size flags by **acceptance**: the switch is consumed from the
//! argv before `init`, so it has no `init`/`system_info` observable, and the case asserts the
//! runtime still boots and a tiny bounded allocation returns its value.
//!
//! One environment case pins a **host** configuration knob rather than an `ERL_*` variable:
//! `BEAM_RT_GC_STRATEGY=immix` is this runtime's default-OFF opt-in for the mark-region mature
//! generation, read once at start-up and invisible to `init`. The oracle ignores it entirely,
//! so the case pins only that the opt-in does **not** perturb observable semantics for an
//! allocation that really grows the process heap (the runtime has no CLI or `system_info`
//! surface for the strategy; `beam_rt`'s `tests/gc_strategy.rs` pins the selector itself).
//!
//! The widened recording surface at the end of this file (`flags/hidden_*` … `flags/pz_*`)
//! pins the remaining command-line shapes of A.6: a recognized distribution/launcher switch with
//! no value (`-hidden`) or one value (`-remsh`/`-connect_all`/`-smp`/`-start_epmd`/`-mode`); the
//! `-s` and `-run` Start instructions, which are consumed before `init` exactly as `-eval` is (so
//! `init:get_argument/1` answers `error` for their names); a second application-env flag
//! (`-mnesia K V`); the value-list boundary (a value list ends at the *next switch*, including a
//! `+`-flag); flag-name spelling (`=` is kept verbatim in the name, a name may contain `.`, names
//! are case-sensitive); empty values; `-kernel` given twice; the interleaved `-pa`/`-pz`
//! occurrence order; and the documented `+P`/`+Q`/`+S` minima. Every golden is the live oracle's
//! output (OTP 29.1.1 / erts-17.1), verified with `differential run <id>`.
//!
//! The tail of the group adds the remaining switch *effects* observed through `system_info/1`:
//! the `+S Sched:SchedOnline` form (`+S 4:2` → `schedulers_online` = 2), `+t N` (`atom_limit`),
//! `+e N` (`ets_limit` — a request above the default raises the limit, one below it is clamped
//! to the default floor 8192), `+B i` (`break_ignored`, with its no-switch contrast) and
//! `+sbt db` (`thread_no_node_processor_spread`, beside the pre-existing `+sbt u` case); the
//! `-env VAR VALUE` launcher switch, which is consumed before `init` and therefore recorded
//! **neither** under its own name nor under the variable's (its *effect* — the variable being
//! set in the child's environment — is pinned separately, through `os:getenv/1`); the
//! implied `.` entry of the default code path; a multi-value `-setcookie`/`-pa`/`-pz`
//! occurrence; and a non-existent `-pa`/`-pz` directory (dropped rather than stored).
//!
//! The final block widens that tail: the remaining `+sbt <mode>` binding-mode spellings
//! (`ns`/`ts`/`ps`/`tnnps` → their `scheduler_bind_type` atoms), the unvalidated `+K` *value*
//! surface (`+K false` and an arbitrary `+K bogus` are accepted, kernel poll stays on, and the
//! switch is still not an `init` argument), the other `-smp` occurrences (`auto`, and the bare
//! valueless form), the **effect** of `-env VAR VALUE` (the variable really is set in the
//! child — observed through `os:getenv/1` — while an unrelated variable stays `false`), a
//! `-pa`/`-pz` entry ending in `/ebin` (recorded verbatim: never re-split, never absolutized),
//! a repeated `-boot` (two occurrences), and the two Start-instruction shapes that must not
//! leak their expression into `init:get_plain_arguments/0` (a `-s Mod Func Arg` and a second
//! `-eval`). `-smp disable` is *not* pinnable: the launcher rejects it before the emulator
//! starts, so no probe observable exists (see `divergences.md`).
//!
//! The very last block widens the `-env` launcher switch and the `--` boundary, and re-probes
//! the `ERL_*FLAGS`-carried switch surface. Pinned: a repeated `-env VAR VALUE` for one variable
//! (the launcher applies each pair in turn, so the **last** value wins in the child); the pair
//! as a two-token value list (the switch that follows it is still recorded); an `-env` *name*
//! containing `=` (it splits at the first `=`, the remainder joining the value, so
//! `-env X=Y hello` sets `X` to `Y=hello`); the `--` end-of-emulator-options token (not itself
//! an `init` argument, and transparent — a `-switch` after it is still recorded, not turned into
//! a plain argument); and an `-env` pair / an `-eval` instruction **carried by** `ERL_AFLAGS` /
//! `ERL_FLAGS` (consumed in the merged argv, so the flag that follows it is still recorded).
//!
//! Four neighbouring shapes do **not** agree at this revision and are handoffs to the divergence
//! ledger (owner `beam_rt`), not cases: (1) a **bare non-switch** argv token (a bare `foo`),
//! where the oracle drops `-noshell` and starts the stock shell on its host-dependent banner
//! while this runtime ignores the token and evaluates the probe; (2) a **plain argument after
//! `--`**, which the oracle reports through `init:get_plain_arguments/0` while this runtime
//! drops it (the `--` token *alone* is pinned, above); (3) a `+`-flag with a **missing operand**
//! (`+S`/`+P`/`+Q`), where both sides consume the next token and agree on stdout/exit but the
//! oracle prints the whole `erts_usage()` block on stderr while this runtime prints one
//! `beam_rt:` line; and (4) an **output-producing extra instruction** — a second `-eval`, or an
//! `-eval` carried by `ERL_FLAGS` (which merges *after* the command line) — where the oracle
//! evaluates every instruction in argv order while this runtime evaluates only the last.
//!
//! **Why no `-pz`-shadowing case lives in this file.** `-pz D` must *append* `D`, so a
//! `proplists.beam` living in `D` must not shadow the stdlib module: `code:which(proplists)`
//! still answers with the stdlib path. A plain `-`-switch case cannot express that, because
//! `code:which/1` — and its only faithful alternative, `code:which(proplists) =:=
//! filename:join(code:lib_dir(stdlib), "ebin/proplists.beam")` — answers with an
//! *installation-absolute* path, so there is no host-independent golden; and no expression
//! case can put a shadow *file* on the code path in the first place. The property is
//! nevertheless pinned, in the one shape the harness can express: the `ModuleEnv` case
//! `flags_apply/erl_aflags_pz_does_not_shadow_a_stdlib_module`, whose compiled directory
//! carries the shadow and whose module reduces `code:which/1` to an integer. (The `-pa` half
//! of the ordering is *not* pinnable the same way: a shadow that wins is loaded during the
//! oracle's boot and crashes it, leaving an empty stdout instead of a comparable golden.)
use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.6 cases: the `-noshell` flag and an arbitrary user flag.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "flags/noshell_argument",
            description: "the emulator argv (which always carries -noshell) is visible to init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(noshell)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/custom_argument",
            description: "an unknown -flag with a value is stored verbatim for init:get_argument/1",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_test_flag)",
                extra_argv: &["-differential_test_flag", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[104,101,108,108,111]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/bare_flag_has_no_value",
            description: "a bare -flag with no argument is stored as an empty value list",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_bare_flag)",
                extra_argv: &["-differential_bare_flag"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/absent_argument_is_error",
            description: "an argument that was not passed is reported as the atom `error`",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_absent_flag)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plain_arguments_are_empty",
            description: "init:get_plain_arguments/0 excludes every modelled switch",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_plain_arguments()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pa_argument_is_stored",
            description: "a -pa code-path switch is recorded verbatim for init:get_argument/1",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pa)",
                extra_argv: &["-pa", "/differential/xyz"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {ok,[[[47,100,105,102,102,101,114,101,110,116,105,97,108,47,120,121,122]]]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pa_multiple_occurrences",
            description: "each -pa switch is a separate occurrence, in argv order",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pa)",
                extra_argv: &["-pa", "/a", "-pa", "/b"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[47,97]],[[47,98]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pa_occurrence_order_is_preserved",
            description: "the -pa occurrences keep the order they were given in",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pa)",
                extra_argv: &["-pa", "/b", "-pa", "/a"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[47,98]],[[47,97]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pa_and_pz_are_distinct_flags",
            description: "an interleaved -pz is a separate flag and does not join the -pa occurrences",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pa)",
                extra_argv: &["-pa", "/a", "-pz", "/z", "-pa", "/b"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[47,97]],[[47,98]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/repeated_flag_records_each_occurrence",
            description: "a flag given twice is two occurrences, each with its own value",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_repeat)",
                extra_argv: &[
                    "-differential_repeat",
                    "a",
                    "-differential_repeat",
                    "b",
                ],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97]],[[98]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/multiple_values_for_one_occurrence",
            description: "one flag occurrence collects every following value token",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_multi)",
                extra_argv: &["-differential_multi", "one", "two", "three"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[111,110,101],[116,119,111],[116,104,114,101,101]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/repeated_bare_flag_is_two_empty_occurrences",
            description: "a bare flag repeated twice is two valueless occurrences",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_bare)",
                extra_argv: &["-differential_bare", "-differential_bare"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[],[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/setcookie_switch_value",
            description: "a recognized -setcookie switch is recorded like any other flag",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(setcookie)",
                extra_argv: &["-setcookie", "cookie123"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[99,111,111,107,105,101,49,50,51]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/sname_switch_value",
            description: "the -sname short node name is observable through init:get_argument/1",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(sname)",
                extra_argv: &["-sname", "mynode"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[109,121,110,111,100,101]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/name_switch_value",
            description: "the -name long node name is observable through init:get_argument/1",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(name)",
                extra_argv: &["-name", "node@host"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[110,111,100,101,64,104,111,115,116]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/application_env_style_flag",
            description: "an `-app key val` application-env flag is recorded verbatim for init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(myapp)",
                extra_argv: &["-myapp", "mykey", "myval"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[109,121,107,101,121],[109,121,118,97,108]]]}"),
                exit_code: 0,
            },
        },
        // --- emulator `+`-flags: observable through system_info, never through init --------
        // A `+`-flag is consumed by the emulator's own argv parser and is *not* recorded as
        // an `init` argument (verified: `init:get_argument/1` answers `error` for each), so
        // the two halves below pin both sides of that rule.
        Case {
            id: "flags/plus_p_sets_process_limit",
            description: "the +P emulator flag changes the observable process_limit",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(process_limit)",
                extra_argv: &["+P", "2048"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2048"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_k_sets_kernel_poll",
            description: "the +K emulator flag changes the observable kernel_poll setting",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(kernel_poll)",
                extra_argv: &["+K", "true"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_sbt_sets_scheduler_bind_type",
            description: "the +sbt emulator flag changes the observable scheduler_bind_type",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(scheduler_bind_type)",
                extra_argv: &["+sbt", "u"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok unbound"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_p_is_not_an_init_argument",
            description: "a +P emulator flag is consumed by the emulator and not recorded by init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('P')",
                extra_argv: &["+P", "2048"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_q_is_not_an_init_argument",
            description: "a +Q emulator flag is consumed by the emulator and not recorded by init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('Q')",
                extra_argv: &["+Q", "4096"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        // --- `+Q N` → `port_limit`: the emulator's port-table sizing ------------------------
        // `+Q N` (and `ERL_MAX_PORTS`) request a port-table size; the emulator rounds it up to
        // the next power of two and caps it at the hard ceiling (`erts_ptab_init_table` /
        // `erts_ptab_max`), which `erlang:system_info(port_limit)` reports. Every golden below
        // was taken from the system oracle (OTP 29.1.1 / erts-17.1) with `differential run
        // <id>`; the no-`+Q` default is pinned separately by `system_info/port_limit`.
        Case {
            id: "flags/plus_q_sets_port_limit",
            description: "the +Q emulator flag changes the observable port_limit to the requested size",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(port_limit)",
                extra_argv: &["+Q", "4096"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 4096"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_q_rounds_a_request_up_to_the_next_power_of_two",
            description: "a non-power-of-two +Q request (2000) is rounded up to the next power of two (2048)",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(port_limit)",
                extra_argv: &["+Q", "2000"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2048"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_q_just_above_a_power_of_two_doubles_it",
            description: "a +Q request one over a power of two (1025) rounds to 2048",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(port_limit)",
                extra_argv: &["+Q", "1025"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2048"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_q_3000_rounds_up_to_4096",
            description: "a +Q request of 3000 rounds up to 4096",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(port_limit)",
                extra_argv: &["+Q", "3000"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 4096"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_q_at_the_hard_ceiling",
            description: "a +Q request at the hard ceiling (134217727) is reported verbatim",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(port_limit)",
                extra_argv: &["+Q", "134217727"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 134217727"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/erl_max_ports_sets_port_limit",
            description: "ERL_MAX_PORTS requests a port-table size through the same channel as +Q",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::EvalEnv {
                expr: "erlang:system_info(port_limit)",
                extra_argv: &[],
                env: &[("ERL_MAX_PORTS", "3000")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 4096"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_k_is_not_an_init_argument",
            description: "a +K emulator flag is consumed by the emulator and not recorded by init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('K')",
                extra_argv: &["+K", "true"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_sbt_is_not_an_init_argument",
            description: "a +sbt emulator flag is consumed by the emulator and not recorded by init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('sbt')",
                extra_argv: &["+sbt", "u"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/boot_switch_value",
            description: "a recognized -boot switch is recorded verbatim for init:get_argument/1",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(boot)",
                extra_argv: &["-boot", "start_clean"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[115,116,97,114,116,95,99,108,101,97,110]]]}"),
                exit_code: 0,
            },
        },
        // --- `+hmax` and the max-heap family: bounded-allocation acceptance ----------------
        // `+hmax Size` (with its `+hmaxk`/`+hmaxel`/`+hmaxib` companions) and the `+hms`/`+hmbs`
        // min-heap / min-binary-heap flags are a `max_heap_size` request the emulator consumes
        // from the argv *before* `init` ever sees it, so -- exactly like `+P`/`+K`/`+sbt`/`+Q` --
        // they are not observable through `init:get_argument/1`. The rewrite must therefore
        // *accept* each one and keep running: the pinned observable is that a small, strictly
        // bounded allocation (`length(lists:seq(1,1000))`, a few dozen KB at most) still yields
        // the value both runtimes produce without the flag. Every allocation here is
        // deliberately tiny so a runner started *with* a heap cap is never itself killed. Each
        // golden is the live oracle's output (OTP 29.1.1 / erts-17.1).
        //
        // A `system_limit` *boundary* case (allocate past the cap under `+hmax <small>`) is
        // deliberately **not** pinned: `+hmax` defaults to `kill => true`, so the oracle *kills*
        // the process on overflow (verified: `erl +hmax 1048576 -noshell -eval
        // 'io:format("~w~n",[length(lists:seq(1,1000000))]),halt(0).'` prints nothing and exits
        // 1) while this runtime's evaluator raises a *catchable* `error:system_limit` that the
        // probe reports (exit 1, one `exception error system_limit` line). That is a plain
        // differing observable, never a capability gap, so only the `+hmax` *acceptance* cases
        // above are carried instead of guessing at a boundary shape.
        Case {
            id: "flags/plus_hmax_bounded_allocation",
            description: "the +hmax emulator flag is accepted and a bounded allocation still succeeds",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "length(lists:seq(1,1000))",
                extra_argv: &["+hmax", "1048576"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1000"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_hmax_zero_is_unlimited",
            description: "a +hmax 0 request (unlimited max heap size) leaves a bounded allocation succeeding",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "length(lists:seq(1,1000))",
                extra_argv: &["+hmax", "0"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1000"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_hmaxk_kill_is_accepted",
            description: "the +hmaxk max-heap kill switch is accepted and a bounded allocation still succeeds",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "length(lists:seq(1,1000))",
                extra_argv: &["+hmaxk", "true"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1000"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_hmaxel_error_logger_is_accepted",
            description: "the +hmaxel max-heap error-logger switch is accepted and a bounded allocation still succeeds",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "length(lists:seq(1,1000))",
                extra_argv: &["+hmaxel", "true"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1000"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_hmaxib_binary_inclusion_is_accepted",
            description: "the +hmaxib max-heap binary-inclusion switch is accepted and a bounded allocation still succeeds",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "length(lists:seq(1,1000))",
                extra_argv: &["+hmaxib", "true"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1000"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_hms_min_heap_size_is_accepted",
            description: "the +hms minimum heap-size flag is accepted and a bounded allocation still succeeds",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "length(lists:seq(1,1000))",
                extra_argv: &["+hms", "233"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1000"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_hmbs_min_binary_heap_size_is_accepted",
            description: "the +hmbs minimum binary-heap-size flag is accepted and a bounded allocation still succeeds",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "length(lists:seq(1,1000))",
                extra_argv: &["+hmbs", "8192"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1000"),
                exit_code: 0,
            },
        },
        // --- environment variables (`ERL_*FLAGS`): the merge, pinned end to end -----------
        // `erl` merges `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS]
        // [ERL_ZFLAGS]` into the argv *before* `init` ever sees it (erlexec.c
        // `initial_argv_massage`), so a tokenized environment switch is indistinguishable
        // from a command-line one. Both runtimes are spawned with the **same** argv *and*
        // the **same** environment (see `CaseKind::EvalEnv`), so only their merge can
        // differ. `ERL_OTP<sysver>_FLAGS` is deliberately **not** pinned: the oracle here is
        // OTP 29 (it reads `ERL_OTP29_FLAGS`) while this runtime targets OTP 30
        // (`ERL_OTP30_FLAGS`), so an exact assertion on either name would be a guaranteed
        // false mismatch. Every golden below is the live oracle's output (OTP 29.1.1).
        Case {
            id: "flags/env_erl_aflags_is_merged",
            description: "an ERL_AFLAGS switch is merged into the argv before the -eval probe observes it",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_env_flag)",
                extra_argv: &[],
                env: &[("ERL_AFLAGS", "-differential_env_flag ajuda")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97,106,117,100,97]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_flags_is_merged",
            description: "an ERL_FLAGS switch is merged into the argv and observed by the -eval probe",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_env_flag)",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "-differential_env_flag envhelp")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[101,110,118,104,101,108,112]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_flags_tokenizes_a_quoted_value",
            description: "an ERL_FLAGS quoted token reaches the -eval probe as one value with the quotes stripped",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_env_flag)",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "-differential_env_flag \"two words\"")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[116,119,111,32,119,111,114,100,115]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_precedence_aflags_cmdline_flags",
            description: "the merge order is [ERL_AFLAGS] [command line] [ERL_FLAGS], pinned by the -eval-visible occurrence order",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_order)",
                extra_argv: &["-differential_order", "cmdline"],
                env: &[
                    ("ERL_AFLAGS", "-differential_order aflag"),
                    ("ERL_FLAGS", "-differential_order fflag"),
                ],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {ok,[[[97,102,108,97,103]],[[99,109,100,108,105,110,101]],[[102,102,108,97,103]]]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_flags_precedes_erl_zflags",
            description: "ERL_FLAGS is merged before ERL_ZFLAGS, both appended after the command line",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_order)",
                extra_argv: &[],
                env: &[
                    ("ERL_FLAGS", "-differential_order fflag"),
                    ("ERL_ZFLAGS", "-differential_order zflag"),
                ],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[102,102,108,97,103]],[[122,102,108,97,103]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_cmdline_overrides_erl_aflags",
            description: "a later command-line +P overrides the earlier ERL_AFLAGS +P (the effective process_limit)",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::EvalEnv {
                expr: "erlang:system_info(process_limit)",
                extra_argv: &["+P", "2048"],
                env: &[("ERL_AFLAGS", "+P 8192")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2048"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_flags_overrides_cmdline",
            description: "a later ERL_FLAGS +P overrides the earlier command-line +P (the effective process_limit)",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::EvalEnv {
                expr: "erlang:system_info(process_limit)",
                extra_argv: &["+P", "2048"],
                env: &[("ERL_FLAGS", "+P 8192")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 8192"),
                exit_code: 0,
            },
        },
        // --- the runtime's own GC-strategy opt-in: configuration by environment ------------
        // `BEAM_RT_GC_STRATEGY=immix` is a **host** knob (not an `ERL_*` variable and not an
        // init argument): the runtime reads it once at start-up and the *oracle ignores it*, so
        // the only differential property it has is that selecting the mark-region mature
        // generation does not perturb observable semantics. The probe therefore allocates
        // enough (`lists:seq/2` builds 100000 cons cells) that the process heap really grows and
        // the collector runs, and asserts the result is unchanged. The selector itself — the
        // break-even rule that maps the raised mature size onto `MatureKind::Immix` — is pinned
        // inside the runtime by `beam_rt`'s `tests/gc_strategy.rs`, which can read a live
        // process's collector; no CLI/`system_info` surface exposes it.
        Case {
            id: "flags/env_gc_strategy_immix_keeps_observable_semantics",
            description: "the default-OFF BEAM_RT_GC_STRATEGY=immix opt-in is accepted and leaves a heap-growing allocation unchanged",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "length(lists:seq(1,100000))",
                extra_argv: &[],
                env: &[("BEAM_RT_GC_STRATEGY", "immix")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 100000"),
                exit_code: 0,
            },
        },
        // --- the wider `-`-switch / `+`-flag surface ---------------------------------------
        // Beyond the `-pa`/`-pz` multiplicity cases above, the A.6 *recording* surface has
        // several more occurrence and application-env shapes: the `-eval` Start instruction
        // (consumed before `init`, so `init:get_argument(eval)` is the bare atom `error`), a
        // lone `-pz`, the `-mode` switch, an `-app key value` application-env flag
        // (`-kernel K V`), a recognized switch given twice, the `-M…` allocator form, the
        // `+S`/`+A` emulator flags observed through `system_info/1` (and their absence from
        // `init`). Every golden is the live oracle's output (OTP 29.1.1 / erts-17.1), verified
        // with `differential run <id>`.
        Case {
            id: "flags/eval_instruction_is_not_an_init_argument",
            description: "the -eval Start instruction is consumed before init and is not recorded",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // The harness itself always passes `-eval <probe>`, so this case needs no extra
                // argv: the already-present Start instruction is the observation.
                expr: "init:get_argument(eval)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pz_alone_is_recorded",
            description: "a lone -pz code-path switch is recorded verbatim like -pa",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pz)",
                extra_argv: &["-pz", "/z"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[47,122]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/mode_flag_is_recorded",
            description: "the -mode start-up switch is recorded verbatim for init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(mode)",
                extra_argv: &["-mode", "embedded"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[101,109,98,101,100,100,101,100]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/kernel_application_env_flag_is_recorded",
            description: "an `-kernel key value` application-env flag is recorded with both values",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(kernel)",
                extra_argv: &["-kernel", "my_key", "my_val"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[109,121,95,107,101,121],[109,121,95,118,97,108]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/setcookie_twice_is_two_occurrences",
            description: "a recognized -setcookie switch given twice is two separate occurrences",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(setcookie)",
                extra_argv: &["-setcookie", "aaa", "-setcookie", "bbb"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97,97,97]],[[98,98,98]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/allocator_flag_is_recorded_as_init_argument",
            description: "an -M<NAME> allocator switch is recorded under its own NAME",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('Mfoo')",
                extra_argv: &["-Mfoo", "1"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[49]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_a_is_not_an_init_argument",
            description: "a +A emulator flag is consumed by the emulator and not recorded by init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('A')",
                extra_argv: &["+A", "5"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_s_is_not_an_init_argument",
            description: "a +S emulator flag is consumed by the emulator and not recorded by init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('S')",
                extra_argv: &["+S", "3"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_s_sets_scheduler_count",
            description: "the +S emulator flag changes the observable normal-scheduler count",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(schedulers)",
                extra_argv: &["+S", "3"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_a_sets_thread_pool_size",
            description: "the +A emulator flag changes the observable async-thread pool size",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(thread_pool_size)",
                extra_argv: &["+A", "5"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 5"),
                exit_code: 0,
            },
        },
        // --- the wider `ERL_*FLAGS` merge surface ------------------------------------------
        // The merge is `[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [command line] [ERL_FLAGS]
        // [ERL_ZFLAGS]`; the cases below widen the already-pinned shape set to a valueless
        // token, several values for one occurrence, a single-quoted token, `ERL_ZFLAGS` as
        // the only source, and the two ends of the chain naming the same switch (which is
        // what makes the "later source wins" order observable as *two* occurrences).
        Case {
            id: "flags/env_erl_aflags_bare_flag_is_valueless",
            description: "an ERL_AFLAGS token with no following value is one valueless occurrence",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_bare_env)",
                extra_argv: &[],
                env: &[("ERL_AFLAGS", "-differential_bare_env")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_flags_multiple_values_one_occurrence",
            description: "one ERL_FLAGS occurrence collects every following value token",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_multi_env)",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "-differential_multi_env one two")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[111,110,101],[116,119,111]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_zflags_alone_is_merged",
            description: "ERL_ZFLAGS is the last merge source and is applied on its own",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_zflag)",
                extra_argv: &[],
                env: &[("ERL_ZFLAGS", "-differential_zflag zval")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[122,118,97,108]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_flags_tokenizes_a_single_quoted_value",
            description: "a single-quoted ERL_FLAGS token is one value with the quotes stripped",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_multi_env)",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "-differential_multi_env 'a b'")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97,32,98]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_flags_precedes_erl_aflags_for_the_same_switch",
            description: "at one switch the merge renders the ERL_AFLAGS then the ERL_FLAGS occurrence",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_ovr)",
                extra_argv: &[],
                env: &[
                    ("ERL_AFLAGS", "-differential_ovr aflag"),
                    ("ERL_FLAGS", "-differential_ovr fflag"),
                ],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97,102,108,97,103]],[[102,102,108,97,103]]]}"),
                exit_code: 0,
            },
        },
        // --- the widened recording surface: recognized switches, Start instructions, name
        // --- spelling, value-list boundaries, and the +P/+Q/+S minima ----------------------
        // A recognized switch that takes no value is still an *occurrence*: `-hidden` (a
        // distribution switch) is recorded as the valueless occurrence `[]`, exactly like an
        // unknown bare `-flag`. A recognized switch that *does* take a value registers it as a
        // single element of that occurrence's value list.
        Case {
            id: "flags/hidden_switch_is_a_valueless_occurrence",
            description: "the valueless -hidden distribution switch is recorded as an empty occurrence",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(hidden)",
                extra_argv: &["-hidden"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/remsh_switch_value",
            description: "the -remsh launcher switch is recorded with its node name",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(remsh)",
                extra_argv: &["-remsh", "foo"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[102,111,111]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/connect_all_switch_value",
            description: "the -connect_all distribution switch is recorded with its boolean value",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(connect_all)",
                extra_argv: &["-connect_all", "false"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[102,97,108,115,101]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/smp_enable_switch_value",
            description: "the -smp scheduler-model switch is recorded with its (accepted) value",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(smp)",
                extra_argv: &["-smp", "enable"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[101,110,97,98,108,101]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/start_epmd_switch_value",
            description: "the -start_epmd launcher switch is recorded with its boolean value",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(start_epmd)",
                extra_argv: &["-start_epmd", "false"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[102,97,108,115,101]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/mode_interactive_switch_value",
            description: "the alternative -mode interactive start-up mode is recorded verbatim",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(mode)",
                extra_argv: &["-mode", "interactive"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[105,110,116,101,114,97,99,116,105,118,101]]]}"),
                exit_code: 0,
            },
        },
        // `-s MOD FUNC [ARG…]` and `-run MOD FUNC [ARG…]` are **Start instructions** consumed by
        // `init` in argv order, exactly like `-eval`: `init` never records them, so
        // `init:get_argument/1` answers `error` for their names. `init get_argument ok` is a
        // harmless target (its result is discarded) that keeps the runtime booting to the probe.
        Case {
            id: "flags/s_instruction_is_not_an_init_argument",
            description: "the -s Start instruction is consumed before init and is not recorded",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(s)",
                extra_argv: &["-s", "init", "get_argument", "ok"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/run_instruction_is_not_an_init_argument",
            description: "the -run Start instruction is consumed before init and is not recorded",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(run)",
                extra_argv: &["-run", "init", "get_argument", "ok"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        // An application-env flag is not special to `kernel`: any `-app key value` form is
        // recorded under the application name with both tokens as its values.
        Case {
            id: "flags/mnesia_application_env_flag_is_recorded",
            description: "an `-mnesia key value` application-env flag is recorded with both values",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(mnesia)",
                extra_argv: &["-mnesia", "dump_log_write_threshold", "100"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {ok,[[[100,117,109,112,95,108,111,103,95,119,114,105,116,101,95,116,104,\
                     114,101,115,104,111,108,100],[49,48,48]]]}",
                ),
                exit_code: 0,
            },
        },
        // The value-list boundary: an occurrence's values are the following non-switch tokens,
        // so the next switch (of any kind) ends the list -- `-dif_b` here, `+K` in the next case.
        Case {
            id: "flags/value_list_stops_at_the_next_switch",
            description: "an occurrence collects only the tokens up to the next switch",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_boundary_a)",
                extra_argv: &[
                    "-differential_boundary_a",
                    "one",
                    "two",
                    "-differential_boundary_b",
                    "three",
                ],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[111,110,101],[116,119,111]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/switch_after_a_value_list_records_its_own_value",
            description: "the switch that ended the previous value list is recorded with its own value",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_boundary_b)",
                extra_argv: &[
                    "-differential_boundary_a",
                    "one",
                    "two",
                    "-differential_boundary_b",
                    "three",
                ],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[116,104,114,101,101]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/value_list_stops_at_an_emulator_flag",
            description: "a +-flag also ends an init argument's value list",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_boundary_e)",
                extra_argv: &["-differential_boundary_e", "one", "+K", "true"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[111,110,101]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/kernel_flag_twice_is_two_occurrences",
            description: "an application-env flag given twice is two occurrences, each with its pair",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(kernel)",
                extra_argv: &["-kernel", "k1", "v1", "-kernel", "k2", "v2"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[107,49],[118,49]],[[107,50],[118,50]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/kernel_flag_with_a_single_value",
            description: "an application-env flag with a lone key is recorded as one value",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(kernel)",
                extra_argv: &["-kernel", "onlykey"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[111,110,108,121,107,101,121]]]}"),
                exit_code: 0,
            },
        },
        // Flag-name spelling: `=` does **not** split a switch (the whole `-dif_eq=v` is the flag
        // name), so `-dif_eq=v` is one *valueless* occurrence under the name `'dif_eq=v'` while a
        // separate `=v` token is an ordinary value. An unknown flag key may contain `.`, and
        // keys are case-sensitive (an uppercase `-DIF` is not the lowercase `dif` key).
        Case {
            id: "flags/equals_in_a_flag_name_is_not_split",
            description: "an `-flag=value` token is one valueless occurrence named `flag=value`",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('differential_eq=v')",
                extra_argv: &["-differential_eq=v"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/equals_prefixed_value_is_recorded_verbatim",
            description: "an `=value` token is an ordinary value with the `=` kept",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_eq)",
                extra_argv: &["-differential_eq", "=v"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[61,118]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/a_flag_key_may_contain_a_dot",
            description: "an unknown flag key may contain a dot and is recorded verbatim",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('differential.key')",
                extra_argv: &["-differential.key", "v"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[118]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/a_flag_key_is_case_sensitive",
            description: "an uppercase -DIF switch is not the lowercase `dif` key",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(dif)",
                extra_argv: &["-DIF", "v"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        // Empty values: an occurrence may carry an empty value, alone or beside a non-empty one.
        Case {
            id: "flags/empty_string_value_is_recorded",
            description: "an empty-string value is recorded as one empty value element",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_empty)",
                extra_argv: &["-differential_empty", ""],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/empty_and_nonempty_values_in_one_occurrence",
            description: "one occurrence may hold an empty value beside a non-empty one",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_empty)",
                extra_argv: &["-differential_empty", "", "x"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[],[120]]]}"),
                exit_code: 0,
            },
        },
        // A bare flag immediately followed by a *valued* flag: the first is the valueless
        // occurrence `[]` and the second owns the following token.
        Case {
            id: "flags/bare_flag_before_a_valued_flag",
            description: "a bare flag before a valued flag stays a valueless occurrence",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_bare_a)",
                extra_argv: &["-differential_bare_a", "-differential_bare_b", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/value_after_a_bare_flag_belongs_to_that_flag",
            description: "the token after a bare flag belongs to the following valued flag",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_bare_b)",
                extra_argv: &["-differential_bare_a", "-differential_bare_b", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[104,101,108,108,111]]]}"),
                exit_code: 0,
            },
        },
        // Interleaving `-pz` between `-pa` occurrences must neither join the `-pa` list nor
        // reorder it: each switch keeps its own occurrences, in argv order.
        Case {
            id: "flags/interleaved_pa_occurrences_keep_order",
            description: "interleaved -pz switches leave the -pa occurrences in argv order",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pa)",
                extra_argv: &["-pa", "/a", "-pz", "/z", "-pa", "/b", "-pz", "/w", "-pa", "/c"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[47,97]],[[47,98]],[[47,99]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/interleaved_pz_occurrences_keep_order",
            description: "interleaved -pa switches leave the -pz occurrences in argv order",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pz)",
                extra_argv: &["-pa", "/a", "-pz", "/z", "-pa", "/b", "-pz", "/w", "-pa", "/c"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[47,122]],[[47,119]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/nonadjacent_occurrences_keep_argv_order",
            description: "non-adjacent occurrences of one flag are recorded in argv order",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_spread)",
                extra_argv: &[
                    "-differential_spread",
                    "1",
                    "-differential_other",
                    "2",
                    "-differential_spread",
                    "3",
                ],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[49]],[[51]]]}"),
                exit_code: 0,
            },
        },
        // The documented *minima* of the sizing flags: `+P` and `+Q` accept 1024, and `+S 1`
        // pins the online scheduler count deterministically (no host CPU count leaks in).
        Case {
            id: "flags/plus_p_sets_the_minimum_process_limit",
            description: "the +P emulator flag accepts its documented minimum (1024)",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(process_limit)",
                extra_argv: &["+P", "1024"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1024"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_q_sets_the_minimum_port_limit",
            description: "the +Q emulator flag accepts its documented minimum (1024)",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(port_limit)",
                extra_argv: &["+Q", "1024"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1024"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_s_one_sets_schedulers_online",
            description: "the +S emulator flag sets the observable online scheduler count",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(schedulers_online)",
                extra_argv: &["+S", "1"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        // The newly-faithful switch surfaces: the `+S Sched:SchedOnline` form (only the online
        // count is observable), the `-env` launcher switch (consumed before `init`, so *not*
        // recorded), a multi-value `-setcookie` occurrence, the implied `.` code-path entry, and
        // the `+t`/`+e`/`+B`/`+sbt` emulator flags, each through its own `system_info/1`
        // observable. `+e` is pinned twice: a request above the default really raises the limit,
        // and one below it is clamped to the documented default floor (8192 on both runtimes).
        Case {
            id: "flags/plus_s_total_online_sets_schedulers_online",
            description: "the +S Sched:SchedOnline form sets the online scheduler count",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                // `+S 4:2` is "4 schedulers, 2 online": `schedulers_online/0` is the observable
                // half (the total is `schedulers/0`, which the host CPU count can influence).
                expr: "erlang:system_info(schedulers_online)",
                extra_argv: &["+S", "4:2"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_switch_is_not_an_init_argument",
            description: "the -env VAR VALUE launcher switch is consumed and never recorded by init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // `-env` is the `erl` launcher's own switch (it sets the variable in the spawned
                // process environment), so `init` never sees it: neither its own name nor its
                // variable name is an argument. (The *effect* — the variable being set — is a
                // divergence and is deliberately NOT pinned; only the recording is.)
                expr: "init:get_argument(env)",
                extra_argv: &["-env", "differential_env_var", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_switch_value_is_not_an_init_argument",
            description: "the -env variable name is not recorded as an init argument either",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_env_var)",
                extra_argv: &["-env", "differential_env_var", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/setcookie_multiple_values_one_occurrence",
            description: "one -setcookie occurrence may carry two values, recorded as one occurrence",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(setcookie)",
                extra_argv: &["-setcookie", "a", "b"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97],[98]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/dot_is_on_the_code_path",
            description: "the current directory is on the default code path",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // `.` is the code path's implied last entry; it is a literal (not a
                // host-dependent absolute path), so the membership answer is portable.
                expr: "lists:member(\".\", code:get_path())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_t_sets_the_atom_table_size",
            description: "the +t emulator flag sets the observable atom-table limit",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(atom_limit)",
                extra_argv: &["+t", "100000"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 100000"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_e_sets_the_ets_table_limit",
            description: "the +e emulator flag raises the observable ETS-table limit",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(ets_limit)",
                extra_argv: &["+e", "10000"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 10000"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_e_request_below_the_default_is_clamped",
            description: "a +e request below the default ETS limit is clamped to the floor",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                // The floor is the default limit (8192) — a constant, not a host property — so
                // this pins the clamp rule itself; `+e 10000` above pins the effective raise.
                expr: "erlang:system_info(ets_limit)",
                extra_argv: &["+e", "10"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 8192"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_b_i_sets_break_ignored",
            description: "the +B i emulator flag makes the break handler ignored",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(break_ignored)",
                extra_argv: &["+B", "i"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_b_default_is_not_ignored",
            description: "without +B the break handler is enabled, so break_ignored is false",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                // The contrast case for `flags/plus_b_i_sets_break_ignored`: the same observable
                // without the switch, so the `+B i` effect is unambiguous.
                expr: "erlang:system_info(break_ignored)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_sbt_db_sets_scheduler_bind_type",
            description: "the +sbt db emulator flag selects thread_no_node_processor_spread",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                // `db` = "default binding, no node/processor spreading" — the same observable as
                // `flags/plus_sbt_sets_scheduler_bind_type` (which pins `u`).
                expr: "erlang:system_info(scheduler_bind_type)",
                extra_argv: &["+sbt", "db"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok thread_no_node_processor_spread"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pa_multiple_values_one_occurrence",
            description: "one -pa occurrence may carry two directories, recorded as one occurrence",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pa)",
                extra_argv: &["-pa", "a", "b"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97],[98]]]}"),
                exit_code: 0,
            },
        },
        // A non-existent code-path directory is dropped rather than stored: `.` is on the path
        // but the bogus literal is not, on either `-pa` (prepend) or `-pz` (append).
        Case {
            id: "flags/pa_nonexistent_dir_is_not_on_the_code_path",
            description: "a -pa directory that does not exist never reaches code:get_path/0",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "lists:member(\"/nonexistent-differential-dir\", code:get_path())",
                extra_argv: &["-pa", "/nonexistent-differential-dir"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pz_nonexistent_dir_is_not_on_the_code_path",
            description: "a -pz directory that does not exist never reaches code:get_path/0",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "lists:member(\"/nonexistent-differential-dir\", code:get_path())",
                extra_argv: &["-pz", "/nonexistent-differential-dir"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pz_multiple_values_one_occurrence",
            description: "one -pz occurrence may carry two directories, recorded as one occurrence",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pz)",
                extra_argv: &["-pz", "a", "b"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97],[98]]]}"),
                exit_code: 0,
            },
        },
        // --- the `+K` value surface, the `-smp` model switch, and the launcher switches -----
        // `+K [true|false]` is *not validated* by either runtime: any value is consumed by the
        // emulator before `init` sees the argv, and `erlang:system_info(kernel_poll)` reports
        // the same `true` for `+K true` (the pre-existing `flags/plus_k_sets_kernel_poll`),
        // `+K false` and an arbitrary `+K bogus`. The switch is likewise never recorded by
        // `init`, so `init:get_argument('K')` stays the atom `error` for every value.
        Case {
            id: "flags/plus_k_false_keeps_kernel_poll_on",
            description: "the +K false emulator flag is accepted and kernel poll stays observable",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(kernel_poll)",
                extra_argv: &["+K", "false"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_k_unrecognized_value_is_accepted",
            description: "an arbitrary +K value is accepted verbatim rather than rejected",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(kernel_poll)",
                extra_argv: &["+K", "bogus"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_k_false_is_not_an_init_argument",
            description: "the +K false switch is consumed by the emulator and not recorded by init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // The `+K true` half of the same rule is `flags/plus_k_is_not_an_init_argument`.
                expr: "init:get_argument('K')",
                extra_argv: &["+K", "false"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        // `-smp` is the (historically named) scheduler-model switch. Every accepted value is
        // recorded verbatim as its own occurrence -- including the valueless occurrence `[]`,
        // exactly like any other bare `-`-switch. `-smp disable` is *not* one of them: the
        // launcher rejects it outright ("Argument '-smp disable' not supported. Use \"+S 1\"
        // instead.") so no probe observable exists for it.
        Case {
            id: "flags/smp_auto_switch_value",
            description: "the alternative -smp auto scheduler-model value is recorded verbatim",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(smp)",
                extra_argv: &["-smp", "auto"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97,117,116,111]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/smp_bare_is_a_valueless_occurrence",
            description: "a bare -smp with no value is recorded as the empty occurrence",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(smp)",
                extra_argv: &["-smp"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        // The remaining `+sbt <mode>` binding-mode spellings, each through its own
        // `system_info(scheduler_bind_type)` answer. `u` and `db` are pinned by
        // `flags/plus_sbt_sets_scheduler_bind_type` / `flags/plus_sbt_db_sets_scheduler_bind_type`;
        // these are the other modes the switch documents.
        Case {
            id: "flags/plus_sbt_no_spread_sets_scheduler_bind_type",
            description: "the +sbt ns emulator flag selects no_spread",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(scheduler_bind_type)",
                extra_argv: &["+sbt", "ns"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok no_spread"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_sbt_thread_spread_sets_scheduler_bind_type",
            description: "the +sbt ts emulator flag selects thread_spread",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(scheduler_bind_type)",
                extra_argv: &["+sbt", "ts"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok thread_spread"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_sbt_processor_spread_sets_scheduler_bind_type",
            description: "the +sbt ps emulator flag selects processor_spread",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(scheduler_bind_type)",
                extra_argv: &["+sbt", "ps"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok processor_spread"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plus_sbt_thread_no_node_processor_spread_sets_scheduler_bind_type",
            description: "the +sbt tnnps emulator flag selects thread_no_node_processor_spread",
            contracts: &[Contract::Cli, Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(scheduler_bind_type)",
                extra_argv: &["+sbt", "tnnps"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok thread_no_node_processor_spread"),
                exit_code: 0,
            },
        },
        // The `-env VAR VALUE` launcher switch is consumed before `init` (see the two
        // `flags/env_switch_*` recording cases) **and applied**: the variable really exists in
        // the child's environment, so `os:getenv/1` answers the value (a string, whose `~w`
        // image is a byte list) — and an unrelated variable stays `false`, so the case cannot
        // pass by a runtime that leaks the raw argv into the environment.
        Case {
            id: "flags/env_variable_switch_sets_the_variable",
            description: "the -env VAR VALUE launcher switch really sets the variable in the child",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "os:getenv(\"differential_env_var\")",
                extra_argv: &["-env", "differential_env_var", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,101,108,108,111]"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_variable_switch_leaves_other_variables_unset",
            description: "a variable the -env switch did not name stays unset",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "os:getenv(\"differential_env_absent\")",
                extra_argv: &["-env", "differential_env_var", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        // A `-pa`/`-pz` code-path entry that ends in an `/ebin` suffix is recorded verbatim,
        // component for component -- the switch's value is not normalized (no `ebin`
        // stripping, no absolutization) by either runtime.
        Case {
            id: "flags/pa_dir_ending_in_ebin_is_recorded",
            description: "a -pa directory ending in /ebin is recorded verbatim, unsplit",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pa)",
                extra_argv: &["-pa", "/differential/xyz/ebin"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {ok,[[[47,100,105,102,102,101,114,101,110,116,105,97,108,47,120,121,\
                     122,47,101,98,105,110]]]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pz_dir_ending_in_ebin_is_recorded",
            description: "a -pz directory ending in /ebin is recorded verbatim, unsplit",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pz)",
                extra_argv: &["-pz", "/differential/xyz/ebin"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {ok,[[[47,100,105,102,102,101,114,101,110,116,105,97,108,47,120,121,\
                     122,47,101,98,105,110]]]}",
                ),
                exit_code: 0,
            },
        },
        // A repeated `-boot` is two occurrences, each carrying its own start-script name (the
        // switch is an ordinary recorded one, unlike the `+`-flags and the Start
        // instructions), and a Start instruction's *arguments* are consumed with it: neither
        // `-s Mod Func Arg` nor a second `-eval` leaks its expression into
        // `init:get_plain_arguments/0`.
        Case {
            id: "flags/boot_switch_twice_is_two_occurrences",
            description: "a -boot switch given twice is two occurrences, in argv order",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(boot)",
                extra_argv: &["-boot", "start_clean", "-boot", "start_clean"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {ok,[[[115,116,97,114,116,95,99,108,101,97,110]],[[115,116,97,114,116,\
                     95,99,108,101,97,110]]]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/s_instruction_values_are_not_plain_arguments",
            description: "the -s Start instruction's arguments are consumed with the instruction",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_plain_arguments()",
                extra_argv: &["-s", "init", "get_argument", "ok"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/eval_instruction_expression_is_not_a_plain_argument",
            description: "a preceding -eval instruction's expression is consumed, not a plain argument",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // The harness appends the probe as a second `-eval`, so the argv here is
                // `-noshell -eval ok -eval <probe>`: the first instruction's expression must be
                // consumed with the instruction on both sides.
                expr: "init:get_plain_arguments()",
                extra_argv: &["-eval", "ok"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        // --- the widened `-env` / env-carried switch surface -------------------------------
        // `-env VAR VALUE` is the launcher's own switch (consumed by `erlexec` before `init`),
        // so it is recorded **neither** under its own name nor under the variable's. The
        // remaining shapes are the ones a conforming launcher must accept: an extra `-eval`
        // Start instruction (consumed with its expression, so `init:get_argument(eval)` still
        // answers the bare atom `error`), a **repeated** `-env` for one variable (the last
        // occurrence's value wins in the child's environment, observed through `os:getenv/1`),
        // and the value-list boundary — the pair is consumed blindly, so the *switch* that
        // follows it survives untouched. Every golden is the live oracle's output
        // (OTP 29.1.1 / erts-17.1), verified with `differential run <id>`.
        Case {
            id: "flags/eval_instruction_extra_is_not_an_init_argument",
            description: "a second -eval Start instruction is still not recorded as an init argument",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // The harness appends the probe as a *third* `-eval`, so the argv is
                // `-noshell -eval ok -eval <probe>`: the extra instruction changes nothing about
                // the recording.
                expr: "init:get_argument(eval)",
                extra_argv: &["-eval", "ok"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_switch_repeated_last_value_wins",
            description: "a repeated -env for one variable sets the variable to the last value",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // The launcher applies each `-env VAR VALUE` pair in turn, so the second
                // occurrence overwrites the first in the child's environment (a string, whose
                // `~w` image is a byte list — `v2` is `[118,50]`).
                expr: "os:getenv(\"differential_env_var\")",
                extra_argv: &[
                    "-env",
                    "differential_env_var",
                    "v1",
                    "-env",
                    "differential_env_var",
                    "v2",
                ],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [118,50]"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_switch_value_does_not_swallow_the_next_switch",
            description: "the -env VAR VALUE pair is consumed as two tokens, leaving the next switch intact",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // `-env` takes exactly two tokens; a switch that follows it is still recorded
                // (`-setcookie cookie` here), so the pair's value list cannot be extended.
                expr: "init:get_argument(setcookie)",
                extra_argv: &["-env", "differential_env_var", "hello", "-setcookie", "cookie"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[99,111,111,107,105,101]]]}"),
                exit_code: 0,
            },
        },
        // The environment half of the same surface: a switch **carried by** `ERL_AFLAGS` /
        // `ERL_FLAGS` is merged into the effective argv before `init` sees it, so a bare
        // `-env` pair there is consumed exactly as a command-line one and the `-differential_
        // env_flag` that follows it is still recorded. `-eval 1+1` is the `ERL_AFLAGS`-carried
        // Start instruction (silent, so the merge is observed through the flag that follows).
        // `ERL_FLAGS` is deliberately **not** pinned with an `-eval`: it is merged *after* the
        // command line, which changes which instruction the runtime evaluates first (a
        // divergence, see `divergences.md`). Every golden is the live oracle's output.
        Case {
            id: "flags/env_erl_flags_carries_env_switch",
            description: "an ERL_FLAGS-carried -env pair is consumed as two tokens and the following flag recorded",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_env_flag)",
                extra_argv: &[],
                env: &[(
                    "ERL_FLAGS",
                    "-env differential_env_var hello -differential_env_flag aid",
                )],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97,105,100]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_aflags_carries_env_switch",
            description: "an ERL_AFLAGS-carried -env pair is consumed as two tokens and the following flag recorded",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_env_flag)",
                extra_argv: &[],
                env: &[(
                    "ERL_AFLAGS",
                    "-env differential_env_var hello -differential_env_flag aid",
                )],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97,105,100]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/env_erl_aflags_carries_eval_instruction",
            description: "an ERL_AFLAGS-carried -eval instruction is consumed with its expression",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(differential_env_flag)",
                extra_argv: &[],
                env: &[("ERL_AFLAGS", "-eval 1+1 -differential_env_flag aid")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[97,105,100]]]}"),
                exit_code: 0,
            },
        },
        // The `-env` pair's *name* is taken verbatim: the launcher puts the whole
        // `VAR=VALUE` assignment into the environment, so a name containing `=` splits at the
        // **first** `=` (`-env X=Y hello` sets `X` to `Y=hello`), observed through
        // `os:getenv/1`.
        Case {
            id: "flags/env_switch_name_containing_equals",
            description: "an -env name containing '=' splits at the first '=', the rest becoming part of the value",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "os:getenv(\"differential_env_var\")",
                extra_argv: &["-env", "differential_env_var=suffix", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [115,117,102,102,105,120,61,104,101,108,108,111]"),
                exit_code: 0,
            },
        },
        // The `--` token: it marks the end of the **emulator** options, so it is not itself an
        // `init` argument, and it is *transparent* to the flags after it — a `-switch` there is
        // still a switch (recorded, with its values consumed), not a plain argument. (The bare
        // non-switch token is the neighbouring shape that does **not** agree, and a plain
        // argument *after* `--` is this runtime's own divergence — both are documented handoffs,
        // see `divergences.md`, not cases.)
        Case {
            id: "flags/double_dash_is_not_recorded",
            description: "the -- end-of-emulator-options token is not itself an init argument",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument('--')",
                extra_argv: &["--"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/double_dash_keeps_a_following_switch_a_switch",
            description: "a -switch after -- is still recorded, not turned into a plain argument",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                // `-differential_test_flag hello` after `--` is consumed as an ordinary recorded
                // switch (its value is not leaked into `init:get_plain_arguments/0`).
                expr: "init:get_plain_arguments()",
                extra_argv: &["--", "-differential_test_flag", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.6 flag group: every case decodes the command line through
    //! `init:get_argument/1` with the drop-in `-noshell … -eval` argv, and the pinned
    //! goldens are the exact occurrences a conforming parser must expose.

    use super::{group, CaseKind, Contract, Probe};

    /// Every case decodes the command line through `init:get_argument/1` with the drop-in
    /// `-noshell … -eval` argv, and the pinned goldens are the exact occurrences a conforming
    /// parser must expose.
    ///
    /// Three families are exempt from the `init:get_argument` shape **on purpose**: the
    /// `+`-flags (`+P`/`+K`/`+sbt`) are consumed by the emulator's own argv parser, so the
    /// only way to observe them is the *other* half of the flag contract —
    /// `erlang:system_info/1`. Their goldens are therefore `ok <system_info value>`; the
    /// cases that assert the emulator did *not* leak them into `init` keep the
    /// `init:get_argument` shape. `-boot` is an ordinary recorded switch and keeps the
    /// `{ok,[...]}` golden shape. The `+hmax`/`+hmaxk`/`+hmaxel`/`+hmaxib`/`+hms`/`+hmbs`
    /// family is likewise consumed by the emulator *and* has no `system_info/1` observable,
    /// so it is pinned by **acceptance**: the runtime boots and a tiny bounded allocation
    /// still returns its value (`ok 1000`).
    #[test]
    fn every_case_is_a_cli_eval_case_with_an_init_or_system_info_probe() {
        for case in group() {
            assert!(
                case.contracts == [Contract::Cli]
                    || case.contracts == [Contract::Cli, Contract::Introspection],
                "case {}: A.6 cases are CLI cases (a +-flag case also evidences A.8)",
                case.id
            );
            assert!(
                case.id.starts_with("flags/"),
                "case {} is outside the flags group",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            let (expr, probe) = match case.kind {
                CaseKind::Eval { expr, probe, .. } => (expr, probe),
                CaseKind::EvalEnv { expr, probe, .. } => (expr, probe),
                CaseKind::Module(_) | CaseKind::ModuleEnv { .. } | CaseKind::Command(_) => {
                    panic!("case {}: an A.6 case must be an eval case", case.id)
                }
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
            assert!(
                expr.starts_with("init:get_argument(")
                    || expr == "init:get_plain_arguments()"
                    || expr.starts_with("erlang:system_info(")
                    // The code-path membership probes (the implied `.` entry, and the dropped
                    // non-existent `-pa`/`-pz` directories) are the other half of the flag
                    // contract: a switch is not only *recorded* but *applied*.
                    || expr.starts_with("lists:member(")
                    || expr == "length(lists:seq(1,1000))"
                    // The `BEAM_RT_GC_STRATEGY` host opt-in has no `init`/`system_info`
                    // observable either, so it too is pinned by acceptance — with an
                    // allocation big enough to grow the heap (see `flags/env_gc_strategy_*`).
                    || expr == "length(lists:seq(1,100000))"
                    // `-env VAR VALUE` is the one launcher switch whose *effect* is observable
                    // in the child: `os:getenv/1` answers the value it set (and `false` for a
                    // variable it did not name).
                    || expr.starts_with("os:getenv("),
                "case {}: A.6 is observed through init:get_argument/1, get_plain_arguments/0, \
                 (for a +-flag) erlang:system_info/1, a code-path membership probe, the \
                 os:getenv/1 effect of -env, or (for an argv flag with no other observable) the \
                 bounded-allocation acceptance probe, got {expr}",
                case.id
            );
            // The golden is the valueless `-noshell` occurrence `[]`, the error atom, a
            // list of per-occurrence value lists, the (always empty) plain-argument list, a
            // code-path membership boolean, the byte-list `~w` image of an `os:getenv/1`
            // answer (the `-env` effect), or (for a `+`-flag / `ERL_MAX_PORTS` request) an
            // integer `system_info/1` value.
            let golden = case.golden.stdout.expect("every case pins a golden");
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            let is_int_value = golden
                .strip_prefix("ok ")
                .is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()));
            let is_byte_list = golden
                .strip_prefix("ok [")
                .and_then(|rest| rest.strip_suffix(']'))
                .is_some_and(|rest| {
                    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == ',')
                });
            assert!(
                golden.starts_with("ok {ok,[")
                    || golden == "ok error"
                    || golden == "ok []"
                    || golden == "ok true"
                    || golden == "ok false"
                    || golden == "ok unbound"
                    // The remaining `+sbt` binding-mode atoms.
                    || golden == "ok no_spread"
                    || golden == "ok thread_spread"
                    || golden == "ok processor_spread"
                    || golden == "ok spread"
                    || golden == "ok thread_no_node_processor_spread"
                    || is_int_value
                    || is_byte_list,
                "case {}: unexpected golden {golden:?}",
                case.id
            );
            // An environment-variable case pins either the `ERL_*FLAGS` merge, the
            // `ERL_MAX_PORTS` port-table request, or this runtime's own GC-strategy opt-in
            // (`BEAM_RT_GC_STRATEGY`, which the oracle ignores), so it must declare a non-empty
            // environment drawn from the documented variable set.
            for (name, value) in case.env() {
                assert!(
                    matches!(
                        *name,
                        "ERL_AFLAGS"
                            | "ERL_FLAGS"
                            | "ERL_ZFLAGS"
                            | "ERL_MAX_PORTS"
                            | "BEAM_RT_GC_STRATEGY"
                    ),
                    "case {}: unexpected environment variable {name}",
                    case.id
                );
                assert!(!value.is_empty(), "case {}: empty {name}", case.id);
            }
        }
    }

    /// The `+Q N` → `port_limit` sizing matrix and the `ERL_MAX_PORTS` request through the
    /// same channel: every golden is the system oracle's own answer (OTP 29.1.1 / erts-17.1,
    /// measured with `erl -noshell +Q <N> -eval
    /// 'io:format("~p",[erlang:system_info(port_limit)]), halt(0).'`).
    #[test]
    fn plus_q_cases_pin_the_oracle_port_table_sizing() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");
        for (id, golden) in [
            ("flags/plus_q_sets_port_limit", "ok 4096"),
            (
                "flags/plus_q_rounds_a_request_up_to_the_next_power_of_two",
                "ok 2048",
            ),
            (
                "flags/plus_q_just_above_a_power_of_two_doubles_it",
                "ok 2048",
            ),
            ("flags/plus_q_3000_rounds_up_to_4096", "ok 4096"),
            ("flags/plus_q_at_the_hard_ceiling", "ok 134217727"),
        ] {
            let case = find(id);
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(
                case.extra_argv().first(),
                Some(&"+Q"),
                "case {id}: the request is a +Q flag"
            );
        }
        // The `ERL_MAX_PORTS` request lands on the same sizing through the environment.
        let env = find("flags/erl_max_ports_sets_port_limit");
        assert_eq!(env.env(), &[("ERL_MAX_PORTS", "3000")]);
        assert_eq!(env.golden.stdout, Some("ok 4096"));
        assert!(env.extra_argv().is_empty(), "case {}: no argv flag", env.id);
    }

    /// The `+hmax` max-heap family is pinned by **acceptance**: each flag is a `max_heap_size`
    /// (or minimum-heap-size) request the emulator consumes from the argv -- so `init` never
    /// records it -- and each case asserts the runtime still boots and returns the same tiny
    /// bounded-allocation value. The allocation is deliberately small so a `+hmax`-limited
    /// runner is never itself killed. Every golden is the live oracle's output.
    #[test]
    fn plus_hmax_family_cases_pin_the_oracle_acceptance() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");
        for (id, argv) in [
            (
                "flags/plus_hmax_bounded_allocation",
                &["+hmax", "1048576"][..],
            ),
            ("flags/plus_hmax_zero_is_unlimited", &["+hmax", "0"][..]),
            ("flags/plus_hmaxk_kill_is_accepted", &["+hmaxk", "true"][..]),
            (
                "flags/plus_hmaxel_error_logger_is_accepted",
                &["+hmaxel", "true"][..],
            ),
            (
                "flags/plus_hmaxib_binary_inclusion_is_accepted",
                &["+hmaxib", "true"][..],
            ),
            (
                "flags/plus_hms_min_heap_size_is_accepted",
                &["+hms", "233"][..],
            ),
            (
                "flags/plus_hmbs_min_binary_heap_size_is_accepted",
                &["+hmbs", "8192"][..],
            ),
        ] {
            let case = find(id);
            assert_eq!(case.golden.stdout, Some("ok 1000"), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the emulator flag");
            assert_eq!(
                case.expr(),
                Some("length(lists:seq(1,1000))"),
                "case {id}: the bounded-allocation acceptance probe"
            );
            assert!(
                case.env().is_empty(),
                "case {id}: the +hmax family is a command-line flag, not an env case"
            );
        }
    }

    /// The `ERL_*FLAGS` precedence chain is pinned by the environment-variable cases: the
    /// merge order and the "later source wins" effect are both oracle-derived.
    #[test]
    fn environment_cases_pin_the_erl_flags_precedence_chain() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");

        // The merge order `[ERL_AFLAGS] [command line] [ERL_FLAGS]` is visible as the
        // occurrence order, and `ERL_FLAGS` before `ERL_ZFLAGS` as the trailing pair.
        let order = find("flags/env_precedence_aflags_cmdline_flags");
        assert_eq!(
            order.kind,
            CaseKind::EvalEnv {
                expr: "init:get_argument(differential_order)",
                extra_argv: &["-differential_order", "cmdline"],
                env: &[
                    ("ERL_AFLAGS", "-differential_order aflag"),
                    ("ERL_FLAGS", "-differential_order fflag"),
                ],
                probe: Probe::Value,
            }
        );
        assert_eq!(
            order.golden.stdout,
            Some("ok {ok,[[[97,102,108,97,103]],[[99,109,100,108,105,110,101]],[[102,102,108,97,103]]]}")
        );

        // A single-valued switch (`+P` → `process_limit`) proves the later source wins: the
        // command line overrides `ERL_AFLAGS`, and `ERL_FLAGS` overrides the command line.
        assert_eq!(
            find("flags/env_cmdline_overrides_erl_aflags").golden.stdout,
            Some("ok 2048")
        );
        assert_eq!(
            find("flags/env_erl_flags_overrides_cmdline").golden.stdout,
            Some("ok 8192")
        );

        // Every environment case is an `EvalEnv`; no plain `Eval` case sneaks an env in.
        for case in group() {
            let has_env = !case.env().is_empty();
            assert_eq!(
                has_env,
                matches!(case.kind, CaseKind::EvalEnv { .. }),
                "case {}: env case/non-env case mismatch",
                case.id
            );
        }
    }

    #[test]
    fn repeated_flags_are_one_occurrence_per_switch() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");
        assert_eq!(
            find("flags/pa_multiple_occurrences").golden.stdout,
            Some("ok {ok,[[[47,97]],[[47,98]]]}")
        );
        assert_eq!(
            find("flags/pa_occurrence_order_is_preserved").golden.stdout,
            Some("ok {ok,[[[47,98]],[[47,97]]]}")
        );
        assert_eq!(
            find("flags/repeated_bare_flag_is_two_empty_occurrences")
                .golden
                .stdout,
            Some("ok {ok,[[],[]]}")
        );
    }

    /// The widened switch surface (the wider `-`-switch / `+`-flag and `ERL_*FLAGS` blocks):
    /// each new case pins the live oracle's answer for its own argv/environment, so the
    /// occurred/valueless/multi-value forms and the emulator-flag observables stay pinned
    /// individually rather than through the umbrella shape assertion above.
    #[test]
    fn the_widened_switch_surface_pins_the_oracle_recording() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");

        // The recording family: `init:get_argument/1` occurrences. The `-eval` Start
        // instruction and the `+A`/`+S` emulator flags are *not* recorded (the bare atom
        // `error`), while every `-`-switch is -- including an application-env flag.
        for (id, argv, expr, golden) in [
            (
                "flags/eval_instruction_is_not_an_init_argument",
                &[][..],
                "init:get_argument(eval)",
                "ok error",
            ),
            (
                "flags/pz_alone_is_recorded",
                &["-pz", "/z"][..],
                "init:get_argument(pz)",
                "ok {ok,[[[47,122]]]}",
            ),
            (
                "flags/mode_flag_is_recorded",
                &["-mode", "embedded"][..],
                "init:get_argument(mode)",
                "ok {ok,[[[101,109,98,101,100,100,101,100]]]}",
            ),
            (
                "flags/kernel_application_env_flag_is_recorded",
                &["-kernel", "my_key", "my_val"][..],
                "init:get_argument(kernel)",
                "ok {ok,[[[109,121,95,107,101,121],[109,121,95,118,97,108]]]}",
            ),
            (
                "flags/setcookie_twice_is_two_occurrences",
                &["-setcookie", "aaa", "-setcookie", "bbb"][..],
                "init:get_argument(setcookie)",
                "ok {ok,[[[97,97,97]],[[98,98,98]]]}",
            ),
            (
                "flags/allocator_flag_is_recorded_as_init_argument",
                &["-Mfoo", "1"][..],
                "init:get_argument('Mfoo')",
                "ok {ok,[[[49]]]}",
            ),
            (
                "flags/plus_a_is_not_an_init_argument",
                &["+A", "5"][..],
                "init:get_argument('A')",
                "ok error",
            ),
            (
                "flags/plus_s_is_not_an_init_argument",
                &["+S", "3"][..],
                "init:get_argument('S')",
                "ok error",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the recorded argv");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(
                case.env().is_empty(),
                "case {id}: a command-line switch is not an env case"
            );
        }

        // The `+S`/`+A` emulator flags are observable through the *other* half of A.6.
        for (id, argv, expr, golden) in [
            (
                "flags/plus_s_sets_scheduler_count",
                &["+S", "3"][..],
                "erlang:system_info(schedulers)",
                "ok 3",
            ),
            (
                "flags/plus_a_sets_thread_pool_size",
                &["+A", "5"][..],
                "erlang:system_info(thread_pool_size)",
                "ok 5",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::Cli, Contract::Introspection], "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the emulator flag");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(case.env().is_empty(), "case {id}: a +-flag is not an env case");
        }

        // The widened `ERL_*FLAGS` merge: a valueless token, several values in one
        // occurrence, a single-quoted token, `ERL_ZFLAGS` on its own, and the two ends of
        // the chain naming the same switch (two occurrences, A before F).
        for (id, expr, env, golden) in [
            (
                "flags/env_erl_aflags_bare_flag_is_valueless",
                "init:get_argument(differential_bare_env)",
                &[("ERL_AFLAGS", "-differential_bare_env")][..],
                "ok {ok,[[]]}",
            ),
            (
                "flags/env_erl_flags_multiple_values_one_occurrence",
                "init:get_argument(differential_multi_env)",
                &[("ERL_FLAGS", "-differential_multi_env one two")][..],
                "ok {ok,[[[111,110,101],[116,119,111]]]}",
            ),
            (
                "flags/env_erl_zflags_alone_is_merged",
                "init:get_argument(differential_zflag)",
                &[("ERL_ZFLAGS", "-differential_zflag zval")][..],
                "ok {ok,[[[122,118,97,108]]]}",
            ),
            (
                "flags/env_erl_flags_tokenizes_a_single_quoted_value",
                "init:get_argument(differential_multi_env)",
                &[("ERL_FLAGS", "-differential_multi_env 'a b'")][..],
                "ok {ok,[[[97,32,98]]]}",
            ),
            (
                "flags/env_erl_flags_precedes_erl_aflags_for_the_same_switch",
                "init:get_argument(differential_ovr)",
                &[
                    ("ERL_AFLAGS", "-differential_ovr aflag"),
                    ("ERL_FLAGS", "-differential_ovr fflag"),
                ][..],
                "ok {ok,[[[97,102,108,97,103]],[[102,102,108,97,103]]]}",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.env(), env, "case {id}: the merged environment");
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(
                case.extra_argv().is_empty(),
                "case {id}: the switch reaches the argv through the environment only"
            );
            assert!(
                matches!(case.kind, CaseKind::EvalEnv { .. }),
                "case {id}: an environment case is an EvalEnv"
            );
        }
    }

    /// The second half of the widened recording surface: the recognized valueless/valued
    /// switches (`-hidden`/`-remsh`/`-connect_all`/`-smp`/`-start_epmd`/`-mode`), the `-s`/`-run`
    /// Start instructions, the second application-env flag (`-mnesia`), the value-list boundary,
    /// flag-name spelling (a verbatim `=`, a dotted key, case sensitivity), empty values, the
    /// interleaved `-pa`/`-pz` order,
    /// and the `+P`/`+Q`/`+S` minima. Each new case pins the live oracle's answer for **its own**
    /// argv, so a parser regression surfaces on its own case instead of only through the umbrella
    /// shape assertion above.
    #[test]
    fn the_widened_recording_surface_pins_the_oracle_recording() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");
        for (id, argv, expr, golden) in [
            (
                "flags/hidden_switch_is_a_valueless_occurrence",
                &["-hidden"][..],
                "init:get_argument(hidden)",
                "ok {ok,[[]]}",
            ),
            (
                "flags/remsh_switch_value",
                &["-remsh", "foo"][..],
                "init:get_argument(remsh)",
                "ok {ok,[[[102,111,111]]]}",
            ),
            (
                "flags/connect_all_switch_value",
                &["-connect_all", "false"][..],
                "init:get_argument(connect_all)",
                "ok {ok,[[[102,97,108,115,101]]]}",
            ),
            (
                "flags/smp_enable_switch_value",
                &["-smp", "enable"][..],
                "init:get_argument(smp)",
                "ok {ok,[[[101,110,97,98,108,101]]]}",
            ),
            (
                "flags/start_epmd_switch_value",
                &["-start_epmd", "false"][..],
                "init:get_argument(start_epmd)",
                "ok {ok,[[[102,97,108,115,101]]]}",
            ),
            (
                "flags/mode_interactive_switch_value",
                &["-mode", "interactive"][..],
                "init:get_argument(mode)",
                "ok {ok,[[[105,110,116,101,114,97,99,116,105,118,101]]]}",
            ),
            (
                "flags/s_instruction_is_not_an_init_argument",
                &["-s", "init", "get_argument", "ok"][..],
                "init:get_argument(s)",
                "ok error",
            ),
            (
                "flags/run_instruction_is_not_an_init_argument",
                &["-run", "init", "get_argument", "ok"][..],
                "init:get_argument(run)",
                "ok error",
            ),
            (
                "flags/mnesia_application_env_flag_is_recorded",
                &["-mnesia", "dump_log_write_threshold", "100"][..],
                "init:get_argument(mnesia)",
                "ok {ok,[[[100,117,109,112,95,108,111,103,95,119,114,105,116,101,95,116,104,114,\
                 101,115,104,111,108,100],[49,48,48]]]}",
            ),
            (
                "flags/value_list_stops_at_the_next_switch",
                &[
                    "-differential_boundary_a",
                    "one",
                    "two",
                    "-differential_boundary_b",
                    "three",
                ][..],
                "init:get_argument(differential_boundary_a)",
                "ok {ok,[[[111,110,101],[116,119,111]]]}",
            ),
            (
                "flags/switch_after_a_value_list_records_its_own_value",
                &[
                    "-differential_boundary_a",
                    "one",
                    "two",
                    "-differential_boundary_b",
                    "three",
                ][..],
                "init:get_argument(differential_boundary_b)",
                "ok {ok,[[[116,104,114,101,101]]]}",
            ),
            (
                "flags/value_list_stops_at_an_emulator_flag",
                &["-differential_boundary_e", "one", "+K", "true"][..],
                "init:get_argument(differential_boundary_e)",
                "ok {ok,[[[111,110,101]]]}",
            ),
            (
                "flags/kernel_flag_twice_is_two_occurrences",
                &["-kernel", "k1", "v1", "-kernel", "k2", "v2"][..],
                "init:get_argument(kernel)",
                "ok {ok,[[[107,49],[118,49]],[[107,50],[118,50]]]}",
            ),
            (
                "flags/kernel_flag_with_a_single_value",
                &["-kernel", "onlykey"][..],
                "init:get_argument(kernel)",
                "ok {ok,[[[111,110,108,121,107,101,121]]]}",
            ),
            (
                "flags/equals_in_a_flag_name_is_not_split",
                &["-differential_eq=v"][..],
                "init:get_argument('differential_eq=v')",
                "ok {ok,[[]]}",
            ),
            (
                "flags/equals_prefixed_value_is_recorded_verbatim",
                &["-differential_eq", "=v"][..],
                "init:get_argument(differential_eq)",
                "ok {ok,[[[61,118]]]}",
            ),
            (
                "flags/a_flag_key_may_contain_a_dot",
                &["-differential.key", "v"][..],
                "init:get_argument('differential.key')",
                "ok {ok,[[[118]]]}",
            ),
            (
                "flags/a_flag_key_is_case_sensitive",
                &["-DIF", "v"][..],
                "init:get_argument(dif)",
                "ok error",
            ),
            (
                "flags/empty_string_value_is_recorded",
                &["-differential_empty", ""][..],
                "init:get_argument(differential_empty)",
                "ok {ok,[[[]]]}",
            ),
            (
                "flags/empty_and_nonempty_values_in_one_occurrence",
                &["-differential_empty", "", "x"][..],
                "init:get_argument(differential_empty)",
                "ok {ok,[[[],[120]]]}",
            ),
            (
                "flags/bare_flag_before_a_valued_flag",
                &["-differential_bare_a", "-differential_bare_b", "hello"][..],
                "init:get_argument(differential_bare_a)",
                "ok {ok,[[]]}",
            ),
            (
                "flags/value_after_a_bare_flag_belongs_to_that_flag",
                &["-differential_bare_a", "-differential_bare_b", "hello"][..],
                "init:get_argument(differential_bare_b)",
                "ok {ok,[[[104,101,108,108,111]]]}",
            ),
            (
                "flags/interleaved_pa_occurrences_keep_order",
                &["-pa", "/a", "-pz", "/z", "-pa", "/b", "-pz", "/w", "-pa", "/c"][..],
                "init:get_argument(pa)",
                "ok {ok,[[[47,97]],[[47,98]],[[47,99]]]}",
            ),
            (
                "flags/interleaved_pz_occurrences_keep_order",
                &["-pa", "/a", "-pz", "/z", "-pa", "/b", "-pz", "/w", "-pa", "/c"][..],
                "init:get_argument(pz)",
                "ok {ok,[[[47,122]],[[47,119]]]}",
            ),
            (
                "flags/nonadjacent_occurrences_keep_argv_order",
                &[
                    "-differential_spread",
                    "1",
                    "-differential_other",
                    "2",
                    "-differential_spread",
                    "3",
                ][..],
                "init:get_argument(differential_spread)",
                "ok {ok,[[[49]],[[51]]]}",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the recorded argv");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(
                case.env().is_empty(),
                "case {id}: a command-line switch is not an env case"
            );
        }

        // The three sizing-flag *minima* are observable through `system_info/1`, so they carry
        // the A.8 contract beside A.6.
        for (id, argv, expr, golden) in [
            (
                "flags/plus_p_sets_the_minimum_process_limit",
                &["+P", "1024"][..],
                "erlang:system_info(process_limit)",
                "ok 1024",
            ),
            (
                "flags/plus_q_sets_the_minimum_port_limit",
                &["+Q", "1024"][..],
                "erlang:system_info(port_limit)",
                "ok 1024",
            ),
            (
                "flags/plus_s_one_sets_schedulers_online",
                &["+S", "1"][..],
                "erlang:system_info(schedulers_online)",
                "ok 1",
            ),
        ] {
            let case = find(id);
            assert_eq!(
                case.contracts,
                &[Contract::Cli, Contract::Introspection],
                "{id}"
            );
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the emulator flag");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(case.env().is_empty(), "case {id}: a +-flag is not an env case");
        }
    }

    /// The tail of the group: the emulator-flag *effects* (`+S Sched:Online`, `+t`, `+e`, `+B`,
    /// `+sbt db`), the `-env` launcher switch (consumed before `init`, so recorded nowhere), the
    /// implied `.` code-path entry, the code-path switches with several values in one
    /// occurrence, and the dropped non-existent `-pa`/`-pz` directory. Each golden is the live
    /// oracle's own answer for its own argv, so a switch that stopped being *applied* (rather
    /// than merely accepted) surfaces on its own case.
    #[test]
    fn the_switch_effects_pin_the_oracle_answers() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");

        // The `system_info/1` observables: these flags are consumed by the emulator, so the
        // observable is the *other* half of A.6 — an applied value, not a recording.
        for (id, argv, expr, golden) in [
            (
                "flags/plus_s_total_online_sets_schedulers_online",
                &["+S", "4:2"][..],
                "erlang:system_info(schedulers_online)",
                "ok 2",
            ),
            (
                "flags/plus_t_sets_the_atom_table_size",
                &["+t", "100000"][..],
                "erlang:system_info(atom_limit)",
                "ok 100000",
            ),
            (
                "flags/plus_e_sets_the_ets_table_limit",
                &["+e", "10000"][..],
                "erlang:system_info(ets_limit)",
                "ok 10000",
            ),
            (
                "flags/plus_e_request_below_the_default_is_clamped",
                &["+e", "10"][..],
                "erlang:system_info(ets_limit)",
                "ok 8192",
            ),
            (
                "flags/plus_b_i_sets_break_ignored",
                &["+B", "i"][..],
                "erlang:system_info(break_ignored)",
                "ok true",
            ),
            (
                "flags/plus_b_default_is_not_ignored",
                &[][..],
                "erlang:system_info(break_ignored)",
                "ok false",
            ),
            (
                "flags/plus_sbt_db_sets_scheduler_bind_type",
                &["+sbt", "db"][..],
                "erlang:system_info(scheduler_bind_type)",
                "ok thread_no_node_processor_spread",
            ),
        ] {
            let case = find(id);
            assert_eq!(
                case.contracts,
                &[Contract::Cli, Contract::Introspection],
                "{id}"
            );
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the emulator flag");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(case.env().is_empty(), "case {id}: a +-flag is not an env case");
        }

        // The recording family: `-env` is consumed before `init` (so neither its own name nor
        // its variable name is an argument), while a multi-value `-setcookie`/`-pa`/`-pz`
        // occurrence stays **one** occurrence carrying both values.
        for (id, argv, expr, golden) in [
            (
                "flags/env_switch_is_not_an_init_argument",
                &["-env", "differential_env_var", "hello"][..],
                "init:get_argument(env)",
                "ok error",
            ),
            (
                "flags/env_switch_value_is_not_an_init_argument",
                &["-env", "differential_env_var", "hello"][..],
                "init:get_argument(differential_env_var)",
                "ok error",
            ),
            (
                "flags/setcookie_multiple_values_one_occurrence",
                &["-setcookie", "a", "b"][..],
                "init:get_argument(setcookie)",
                "ok {ok,[[[97],[98]]]}",
            ),
            (
                "flags/pa_multiple_values_one_occurrence",
                &["-pa", "a", "b"][..],
                "init:get_argument(pa)",
                "ok {ok,[[[97],[98]]]}",
            ),
            (
                "flags/pz_multiple_values_one_occurrence",
                &["-pz", "a", "b"][..],
                "init:get_argument(pz)",
                "ok {ok,[[[97],[98]]]}",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::Cli], "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the recorded argv");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(
                case.env().is_empty(),
                "case {id}: a command-line switch is not an env case"
            );
        }

        // The code-path family: `.` is implied by default, and a directory that does not exist
        // is dropped instead of stored — on both the prepend (`-pa`) and append (`-pz`) side.
        let nonexistent = "/nonexistent-differential-dir";
        for (id, argv, expr, golden) in [
            (
                "flags/dot_is_on_the_code_path",
                &[][..],
                "lists:member(\".\", code:get_path())",
                "ok true",
            ),
            (
                "flags/pa_nonexistent_dir_is_not_on_the_code_path",
                &["-pa", nonexistent][..],
                "lists:member(\"/nonexistent-differential-dir\", code:get_path())",
                "ok false",
            ),
            (
                "flags/pz_nonexistent_dir_is_not_on_the_code_path",
                &["-pz", nonexistent][..],
                "lists:member(\"/nonexistent-differential-dir\", code:get_path())",
                "ok false",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::Cli], "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the code-path argv");
            assert_eq!(case.expr(), Some(expr), "case {id}");
        }
    }

    /// The `+K` value surface, the `-smp` model switch, the `-env` *effect*, and the
    /// `-pa`/`-pz`/`-boot`/Start-instruction shapes added by the final block: each case pins the
    /// live oracle's answer for its own argv, so the value-acceptance rule, the valueless
    /// occurrence, the verbatim code-path value and the consumed Start-instruction arguments are
    /// each anchored individually.
    #[test]
    fn the_plus_flag_values_and_launcher_switches_pin_the_oracle_answers() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");

        // The `+`-flag observables: `+K` is accepted with *any* value (both `false` and a
        // nonsense value leave kernel poll reported as on), and each `+sbt` mode selects its
        // own `scheduler_bind_type` atom.
        for (id, argv, expr, golden) in [
            (
                "flags/plus_k_false_keeps_kernel_poll_on",
                &["+K", "false"][..],
                "erlang:system_info(kernel_poll)",
                "ok true",
            ),
            (
                "flags/plus_k_unrecognized_value_is_accepted",
                &["+K", "bogus"][..],
                "erlang:system_info(kernel_poll)",
                "ok true",
            ),
            (
                "flags/plus_sbt_no_spread_sets_scheduler_bind_type",
                &["+sbt", "ns"][..],
                "erlang:system_info(scheduler_bind_type)",
                "ok no_spread",
            ),
            (
                "flags/plus_sbt_thread_spread_sets_scheduler_bind_type",
                &["+sbt", "ts"][..],
                "erlang:system_info(scheduler_bind_type)",
                "ok thread_spread",
            ),
            (
                "flags/plus_sbt_processor_spread_sets_scheduler_bind_type",
                &["+sbt", "ps"][..],
                "erlang:system_info(scheduler_bind_type)",
                "ok processor_spread",
            ),
            (
                "flags/plus_sbt_thread_no_node_processor_spread_sets_scheduler_bind_type",
                &["+sbt", "tnnps"][..],
                "erlang:system_info(scheduler_bind_type)",
                "ok thread_no_node_processor_spread",
            ),
        ] {
            let case = find(id);
            assert_eq!(
                case.contracts,
                &[Contract::Cli, Contract::Introspection],
                "{id}"
            );
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the emulator flag");
            assert_eq!(case.expr(), Some(expr), "case {id}");
        }
        // The recording family: the `+K` switch is never an `init` argument whatever its value,
        // `-smp` records each accepted value (including the bare, valueless occurrence), a
        // code-path entry ending in `/ebin` is stored verbatim, and a repeated `-boot` is two
        // occurrences.
        for (id, argv, expr, golden) in [
            (
                "flags/plus_k_false_is_not_an_init_argument",
                &["+K", "false"][..],
                "init:get_argument('K')",
                "ok error",
            ),
            (
                "flags/smp_auto_switch_value",
                &["-smp", "auto"][..],
                "init:get_argument(smp)",
                "ok {ok,[[[97,117,116,111]]]}",
            ),
            (
                "flags/smp_bare_is_a_valueless_occurrence",
                &["-smp"][..],
                "init:get_argument(smp)",
                "ok {ok,[[]]}",
            ),
            (
                "flags/pa_dir_ending_in_ebin_is_recorded",
                &["-pa", "/differential/xyz/ebin"][..],
                "init:get_argument(pa)",
                "ok {ok,[[[47,100,105,102,102,101,114,101,110,116,105,97,108,47,120,121,122,47,\
                 101,98,105,110]]]}",
            ),
            (
                "flags/pz_dir_ending_in_ebin_is_recorded",
                &["-pz", "/differential/xyz/ebin"][..],
                "init:get_argument(pz)",
                "ok {ok,[[[47,100,105,102,102,101,114,101,110,116,105,97,108,47,120,121,122,47,\
                 101,98,105,110]]]}",
            ),
            (
                "flags/boot_switch_twice_is_two_occurrences",
                &["-boot", "start_clean", "-boot", "start_clean"][..],
                "init:get_argument(boot)",
                "ok {ok,[[[115,116,97,114,116,95,99,108,101,97,110]],[[115,116,97,114,116,95,99,\
                 108,101,97,110]]]}",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::Cli], "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the recorded argv");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(case.env().is_empty(), "case {id}: a command-line switch");
        }

        // The Start instructions: their arguments are consumed with the instruction, so the
        // plain-argument list stays empty on both sides.
        for (id, argv) in [
            (
                "flags/s_instruction_values_are_not_plain_arguments",
                &["-s", "init", "get_argument", "ok"][..],
            ),
            (
                "flags/eval_instruction_expression_is_not_a_plain_argument",
                &["-eval", "ok"][..],
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::Cli], "{id}");
            assert_eq!(case.golden.stdout, Some("ok []"), "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the Start instruction");
            assert_eq!(case.expr(), Some("init:get_plain_arguments()"), "case {id}");
        }

        // The `-env` *effect*: the named variable really is set in the child (observed through
        // `os:getenv/1`, whose `~w` image is the value's byte list) and no other variable is.
        let env_argv = &["-env", "differential_env_var", "hello"][..];
        let set = find("flags/env_variable_switch_sets_the_variable");
        assert_eq!(set.contracts, &[Contract::Cli]);
        assert_eq!(set.extra_argv(), env_argv, "the -env argv");
        assert_eq!(set.expr(), Some("os:getenv(\"differential_env_var\")"));
        assert_eq!(set.golden.stdout, Some("ok [104,101,108,108,111]"));
        let unset = find("flags/env_variable_switch_leaves_other_variables_unset");
        assert_eq!(unset.extra_argv(), env_argv, "the -env argv");
        assert_eq!(unset.expr(), Some("os:getenv(\"differential_env_absent\")"));
        assert_eq!(unset.golden.stdout, Some("ok false"));
    }

    /// The widened `-env` / env-carried-switch / `--` surface added by the final block: an extra
    /// `-eval` Start instruction, a repeated `-env` (last value wins), the `-env` pair boundary,
    /// an `-env` name containing `=`, the `--` end-of-emulator-options token, and the
    /// `ERL_AFLAGS`/`ERL_FLAGS`-carried switches. Each case pins the live oracle's answer for its
    /// own argv/environment, so the merge/consumption rules stay anchored individually.
    #[test]
    fn the_widened_env_and_env_carried_switch_surface_pins_the_oracle_answers() {
        let find = |id: &str| group().into_iter().find(|c| c.id == id).expect("case");

        // A second `-eval` Start instruction: consumed with its expression (so it is still not
        // an `init` argument), and a repeated `-env` for one variable: the last value wins.
        for (id, argv, expr, golden) in [
            (
                "flags/eval_instruction_extra_is_not_an_init_argument",
                &["-eval", "ok"][..],
                "init:get_argument(eval)",
                "ok error",
            ),
            (
                "flags/env_switch_repeated_last_value_wins",
                &[
                    "-env",
                    "differential_env_var",
                    "v1",
                    "-env",
                    "differential_env_var",
                    "v2",
                ][..],
                "os:getenv(\"differential_env_var\")",
                "ok [118,50]",
            ),
            (
                "flags/env_switch_value_does_not_swallow_the_next_switch",
                &["-env", "differential_env_var", "hello", "-setcookie", "cookie"][..],
                "init:get_argument(setcookie)",
                "ok {ok,[[[99,111,111,107,105,101]]]}",
            ),
            (
                "flags/env_switch_name_containing_equals",
                &["-env", "differential_env_var=suffix", "hello"][..],
                "os:getenv(\"differential_env_var\")",
                "ok [115,117,102,102,105,120,61,104,101,108,108,111]",
            ),
            (
                "flags/double_dash_is_not_recorded",
                &["--"][..],
                "init:get_argument('--')",
                "ok error",
            ),
            (
                "flags/double_dash_keeps_a_following_switch_a_switch",
                &["--", "-differential_test_flag", "hello"][..],
                "init:get_plain_arguments()",
                "ok []",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::Cli], "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "case {id}");
            assert_eq!(case.golden.exit_code, 0, "case {id}");
            assert_eq!(case.extra_argv(), argv, "case {id}: the argv");
            assert_eq!(case.expr(), Some(expr), "case {id}");
            assert!(case.env().is_empty(), "case {id}: a command-line case");
            assert!(
                matches!(case.kind, CaseKind::Eval { .. }),
                "case {id}: a command-line case is a plain Eval"
            );
        }

        // The env-carried switches: an `-env` pair or an `-eval` instruction carried by
        // `ERL_AFLAGS`/`ERL_FLAGS` is merged into the effective argv before `init` sees it, so it
        // is consumed there and the following `-differential_env_flag` is still recorded.
        for (id, var, value) in [
            (
                "flags/env_erl_flags_carries_env_switch",
                "ERL_FLAGS",
                "-env differential_env_var hello -differential_env_flag aid",
            ),
            (
                "flags/env_erl_aflags_carries_env_switch",
                "ERL_AFLAGS",
                "-env differential_env_var hello -differential_env_flag aid",
            ),
            (
                "flags/env_erl_aflags_carries_eval_instruction",
                "ERL_AFLAGS",
                "-eval 1+1 -differential_env_flag aid",
            ),
        ] {
            let case = find(id);
            assert_eq!(case.contracts, &[Contract::Cli], "{id}");
            assert!(
                case.extra_argv().is_empty(),
                "case {id}: the merge is env-only"
            );
            assert_eq!(case.env(), &[(var, value)], "case {id}: the environment");
            assert_eq!(
                case.expr(),
                Some("init:get_argument(differential_env_flag)"),
                "case {id}"
            );
            assert_eq!(case.golden.stdout, Some("ok {ok,[[[97,105,100]]]}"), "case {id}");
            assert_eq!(case.golden.exit_code, 0, "case {id}");
            assert!(
                matches!(case.kind, CaseKind::EvalEnv { .. }),
                "case {id}: an env-carried switch is an EvalEnv"
            );
        }
    }
}
