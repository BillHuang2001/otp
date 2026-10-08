//! Contract **A.9** — the boot-path **prerequisite primitives**.
//!
//! These are the surfaces the *real* `bootstrap/bin/start.boot` chain relies on the booted
//! system answering: the `erlang:pre_loaded/0` set, the applications the boot script has
//! started (`application:which_applications/0`, `application:loaded_applications/0`), the
//! code server's view of the preloaded `init`, kernel's `error_logger`/`vsn` application
//! entries, the boot script's own `register/2` + `whereis/1` and group-leader-inheritance
//! kernel-process primitives, and the `-boot` init argument (contract A.6) with the
//! **stopping point** of the real boot chain (`proc_lib:start_link/4`'s cross-module
//! `proc_lib:sync_start/2` wait, folded into the engine's typed `blocked` gap ⇒ the one honest
//! `unsupported` case here). Every other case below `agree`s.
//!
//! The case id's `<group>/` segment is a **subdivision, not the Rust file name** (see
//! `super`'s module docs), so these `boot/…` ids living in `boot_prereq.rs` is expected: this
//! group is the boot-path *prerequisite* half of the A.9 surface, while `boot` (in `boot.rs`)
//! holds the boot/version-observability half.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.9 boot-path prerequisite cases, asserted by shape or by a release-stable value.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- Boot-path prerequisite primitives -------------------------------------------
        //
        // The primitives the *real* `bootstrap/bin/start.boot` chain relies on now that the
        // booted system answers them: the preloaded-module set, the applications the boot
        // script has loaded, the code server's view of the preloaded `init`, and the
        // init/boot status. The **named-process** surface (`register/2` + `whereis/1`) and
        // the **group-leader** inheritance rule are the boot script's own kernel-process
        // primitives — a booted system could not start `application_controller` without them.
        Case {
            id: "boot/pre_loaded_length_is_22",
            description: "erlang:pre_loaded/0 is the boot-preloaded module set (22 modules)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "length(erlang:pre_loaded())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 22"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/pre_loaded_exact_list",
            description: "erlang:pre_loaded/0 reproduces the frozen 22-atom preloaded set exactly",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:sort(erlang:pre_loaded()) =:= [atomics,counters,erl_init,erl_prim_loader,erl_tracer,erlang,erts_code_purger,erts_dirty_process_signal_handler,erts_internal,erts_literal_area_collector,erts_trace_cleaner,init,persistent_term,prim_buffer,prim_eval,prim_file,prim_inet,prim_net,prim_socket,prim_zip,socket_registry,zlib]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/which_applications_has_kernel",
            description: "the boot script has started `kernel` (it appears in which_applications/0)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:keymember(kernel, 1, application:which_applications())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/which_applications_is_non_empty",
            description: "application:which_applications/0 is non-empty once the boot script has run",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "length(application:which_applications()) > 0",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/loaded_applications_at_least_two",
            description: "the boot script has loaded at least the `kernel` and `stdlib` applications",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "length([A || {A, _, _} <- application:loaded_applications()]) >= 2",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_is_loaded_init_is_preloaded",
            description: "the booted `init` module is a preloaded module (code:is_loaded/1 -> {file,preloaded})",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:is_loaded(init)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {file,preloaded}"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/init_get_status_reports_started",
            description: "init:get_status/0 reports the started state {starting,started} once booted",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "init:get_status()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {starting,started}"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_env_kernel_error_logger_undefined",
            description: "kernel's `error_logger` env key is unset by the boot script (application:get_env/2 -> undefined)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "application:get_env(kernel, error_logger)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok undefined"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_key_kernel_vsn_is_ok",
            description: "application:get_key/2 for kernel's `vsn` is tagged `ok` (shape, not the release version)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "element(1, application:get_key(kernel, vsn))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ok"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/register_and_whereis_round_trip",
            description: "a boot-script-style named process: register/2 then whereis/1 resolves to the same pid",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin P = spawn(fun() -> receive _ -> ok end end), register(boot_named_probe, P), whereis(boot_named_probe) =:= P end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/spawned_process_inherits_group_leader",
            description: "a spawned process inherits its parent's group leader (the boot script's kernel-process link)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Self = self(), GL = erlang:group_leader(), P = spawn(fun() -> receive {q, From} -> From ! erlang:group_leader() end end), P ! {q, Self}, receive ChildGL -> is_pid(ChildGL) andalso ChildGL =:= GL end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // --- The `-boot` init argument and the boot chain's blocked seam -----------------
        //
        // The black-box CLI records `-boot FILE` as the init argument `boot` (contract A.6)
        // but the Rust runtime does not *execute* the boot script, so the real
        // `bootstrap/bin/start.boot` chain is stopped. `-boot start` is the value both
        // runtimes resolve identically (the oracle appends `.boot`, so the bare `start`, not
        // `start.boot`, is what agrees). The negative shape pins the argument's absence.
        Case {
            id: "boot/boot_flag_is_recorded_as_init_argument",
            description: "`-boot start` is recorded as the init argument `boot` (a booted init surface)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "init:get_argument(boot) =:= {ok, [[\"start\"]]}",
                extra_argv: &["-boot", "start"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/no_boot_flag_is_badarg",
            description: "with no `-boot`, init:get_argument/1 has no `boot` argument (element(1,_) raises badarg)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "element(1, init:get_argument(boot))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // The **stopping point** of the real boot chain, pinned as a typed capability gap.
        // The boot path's `{kernelProcess, application_controller, {application_controller,start}}`
        // directive is reached through `proc_lib:start_link/4`, whose cross-module
        // `proc_lib:sync_start/2` `receive` the engine folds into `blocked()`: a boot-chain
        // child that never acks suspends the process, so the runtime signals the engine's
        // unchanged `blocked` diagnostic on exit 3 (never a wrong result, never a hang) while
        // the live oracle answers the same shape. `erlang:node/0` is a child that returns
        // normally, so the oracle produces a clean, stable line (a crashing child would emit
        // a timestamped crash report and break the golden). NOT corpus/pinned: the sibling
        // boot-path surfaces `application:load/1` / `application:start/1` /
        // `whereis(application_controller)` / `init:get_arguments/0` / `code:priv_dir/1` /
        // `application:get_all_key/1`, which currently diverge from the oracle with a *plain*
        // Erlang reason (`exit noproc` / `undefined` / `badarg`) and would (correctly) be a
        // `mismatch`.
        Case {
            id: "boot/proc_lib_start_link_blocks_the_boot_path",
            description: "the boot chain's proc_lib:start_link/4 wait is a cross-module receive the engine folds into a typed `blocked` capability gap",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin catch proc_lib:start_link(erlang, node, []), ok end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,111,107>>"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, CaseKind, Contract, Probe};

    /// The **boot-path prerequisite primitive** cases this group adds: plain `-eval` cases
    /// (no extra argv) whose oracle golden the Rust runtime reproduces, asserted by shape or
    /// by a release-stable value. The negative `no_boot_flag` shape is included: its golden
    /// is the oracle's own `exception error badarg` line (exit 1).
    const BOOT_PREREQUISITE_CASES: &[&str] = &[
        "boot/pre_loaded_length_is_22",
        "boot/pre_loaded_exact_list",
        "boot/which_applications_has_kernel",
        "boot/which_applications_is_non_empty",
        "boot/loaded_applications_at_least_two",
        "boot/code_is_loaded_init_is_preloaded",
        "boot/init_get_status_reports_started",
        "boot/application_get_env_kernel_error_logger_undefined",
        "boot/application_get_key_kernel_vsn_is_ok",
        "boot/register_and_whereis_round_trip",
        "boot/spawned_process_inherits_group_leader",
        "boot/no_boot_flag_is_badarg",
    ];

    /// The one case here that takes **extra argv**: the `-boot start` CLI flag (contract A.6).
    const BOOT_FLAG_CASES: &[&str] = &["boot/boot_flag_is_recorded_as_init_argument"];

    /// The one honest **capability gap** this group adds: the boot chain's `proc_lib:start_link/4`
    /// wait, which the engine folds into a typed `blocked` diagnostic (exit 3) — classified
    /// `unsupported`, never a `mismatch`.
    const BOOT_GAP_CASES: &[&str] = &["boot/proc_lib_start_link_blocks_the_boot_path"];

    fn case<'a>(cases: &'a [super::Case], id: &str) -> &'a super::Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the boot-prereq group"))
    }

    #[test]
    fn boot_prerequisite_cases_are_registered_as_build_eval_cases() {
        let cases = group();
        let cases = cases.as_slice();
        for id in BOOT_PREREQUISITE_CASES {
            let case = case(cases, id);
            assert_eq!(case.contracts, &[Contract::Build], "case {id}");
            assert!(
                matches!(case.kind, CaseKind::Eval { .. }),
                "case {id} must be an Eval case"
            );
            assert!(
                case.extra_argv().is_empty(),
                "case {id} takes no extra argv"
            );
        }
    }

    #[test]
    fn boot_prerequisite_cases_never_pin_a_volatile_value() {
        let cases = group();
        let cases = cases.as_slice();
        for id in BOOT_PREREQUISITE_CASES.iter().chain(BOOT_FLAG_CASES) {
            let golden = case(cases, id)
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {id} asserts stdout"));
            assert!(
                !golden.contains("<0."),
                "case {id}: the golden pins an unstable pid: {golden:?}"
            );
        }
    }

    #[test]
    fn boot_flag_case_carries_the_boot_argument() {
        let cases = group();
        let cases = cases.as_slice();
        let case = case(cases, BOOT_FLAG_CASES[0]);
        assert_eq!(case.contracts, &[Contract::Build], "the -boot case");
        assert_eq!(
            case.extra_argv(),
            &["-boot", "start"],
            "the -boot case must pass `-boot start` (the oracle appends `.boot`)"
        );
        let CaseKind::Eval { expr, probe, .. } = case.kind else {
            panic!("the -boot case must be an Eval case");
        };
        assert_eq!(probe, Probe::Value, "the -boot case");
        assert!(
            expr.contains("init:get_argument(boot)"),
            "the -boot case must observe the `boot` init argument: {expr:?}"
        );
    }

    #[test]
    fn boot_gap_case_is_a_typed_capability_observation() {
        let cases = group();
        let cases = cases.as_slice();
        let case = case(cases, BOOT_GAP_CASES[0]);
        assert_eq!(case.contracts, &[Contract::Build], "the boot-gap case");
        assert!(
            case.extra_argv().is_empty(),
            "the boot-gap case takes no extra argv"
        );
        let CaseKind::Eval { expr, probe, .. } = case.kind else {
            panic!("the boot-gap case must be an Eval case");
        };
        assert_eq!(probe, Probe::TermToBinary, "the boot-gap case");
        assert!(
            expr.contains("proc_lib:start_link"),
            "the boot-gap case must reach the boot chain's start wait: {expr:?}"
        );
        assert!(
            expr.contains("catch"),
            "the boot-gap case must catch the boot chain's wait: {expr:?}"
        );
        // The golden is the *oracle's* clean `ok` line (never a capability-gap marker), so the
        // Rust side is the one that reports the typed gap.
        assert_eq!(case.golden.stdout, Some("ok <<131,119,2,111,107>>"));
        assert_eq!(case.golden.exit_code, 0);
    }
}
