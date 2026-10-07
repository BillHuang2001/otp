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
//! *Known Issues*).

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
    /// `{ok,[…]}` golden shape.
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
                CaseKind::Module(_) => {
                    panic!("case {}: an A.6 case must be an eval case", case.id)
                }
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
            assert!(
                expr.starts_with("init:get_argument(")
                    || expr == "init:get_plain_arguments()"
                    || expr.starts_with("erlang:system_info("),
                "case {}: A.6 is observed through init:get_argument/1, get_plain_arguments/0 \
                 or (for a +-flag) erlang:system_info/1, got {expr}",
                case.id
            );
            // The golden is the valueless `-noshell` occurrence `[]`, the error atom, a
            // list of per-occurrence value lists, or the (always empty) plain-argument list.
            let golden = case.golden.stdout.expect("every case pins a golden");
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            assert!(
                golden.starts_with("ok {ok,[")
                    || golden == "ok error"
                    || golden == "ok []"
                    || golden == "ok 2048"
                    || golden == "ok 8192"
                    || golden == "ok true"
                    || golden == "ok unbound",
                "case {}: unexpected golden {golden:?}",
                case.id
            );
            // An environment-variable case pins the `ERL_*FLAGS` merge, so it must declare a
            // non-empty environment drawn from the documented variable set.
            for (name, value) in case.env() {
                assert!(
                    matches!(*name, "ERL_AFLAGS" | "ERL_FLAGS" | "ERL_ZFLAGS"),
                    "case {}: unexpected environment variable {name}",
                    case.id
                );
                assert!(!value.is_empty(), "case {}: empty {name}", case.id);
            }
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
