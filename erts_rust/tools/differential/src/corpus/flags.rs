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
//! pinned here; forms that diverge (e.g. plain arguments after `-extra`/`--`, which the
//! oracle treats as `init:get_plain_arguments/0` while this runtime rejects them) are
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
                    || expr == "length(lists:seq(1,1000))"
                    // The `BEAM_RT_GC_STRATEGY` host opt-in has no `init`/`system_info`
                    // observable either, so it too is pinned by acceptance — with an
                    // allocation big enough to grow the heap (see `flags/env_gc_strategy_*`).
                    || expr == "length(lists:seq(1,100000))",
                "case {}: A.6 is observed through init:get_argument/1, get_plain_arguments/0, \
                 (for a +-flag) erlang:system_info/1, or (for an argv flag with no other \
                 observable) the bounded-allocation acceptance probe, got {expr}",
                case.id
            );
            // The golden is the valueless `-noshell` occurrence `[]`, the error atom, a
            // list of per-occurrence value lists, the (always empty) plain-argument list, or
            // (for a `+`-flag / `ERL_MAX_PORTS` request) an integer `system_info/1` value.
            let golden = case.golden.stdout.expect("every case pins a golden");
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            let is_int_value = golden
                .strip_prefix("ok ")
                .is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()));
            assert!(
                golden.starts_with("ok {ok,[")
                    || golden == "ok error"
                    || golden == "ok []"
                    || golden == "ok true"
                    || golden == "ok unbound"
                    || is_int_value,
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
}
