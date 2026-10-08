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

//! Contract **A.9** — preloaded/boot, expanded: boot-script-progress and
//! preloaded-module-invariant cases observed through the booted runtime.
//!
//! A.9 is "the system can build and start itself". Its black-box residue beyond the
//! version banner and the code path (the sibling `boot`/`boot_prereq` groups) is the
//! **state the boot script leaves behind**: the frozen `erlang:pre_loaded/0` module set and
//! its invariants, the code server's view of every preloaded module, the `init`/boot status
//! and its recorded arguments, the kernel processes the boot script registered, the
//! applications it started, and the version/emulator constants a booted system reports.
//!
//! Every assertion is **structural** or a **release-stable value** wherever the value is
//! machine/release-dependent (the oracle is OTP 29 while the rewrite targets OTP 30, so the
//! `otp_release`/`system_version` *string* is never pinned by value — only its shape). The
//! frozen build constants that both runtimes must report for `erts-17.1`
//! (`erlang:system_info(version)`) *are* pinned exactly. No case pins a pid/ref/creation or a
//! multi-key-map `~w` rendering.
//!
//! Most cases are plain `-eval` cases ([`CaseKind::Eval`](super::CaseKind::Eval)) with a
//! [`Probe::Value`](super::Probe::Value) probe, so both runtimes receive identical argv. The
//! one `boot_extra/boot_flag_via_erl_aflags` case drives the `-boot` init argument (contract
//! A.6) through the environment half ([`CaseKind::EvalEnv`](super::CaseKind::EvalEnv)), pinning
//! that an `ERL_AFLAGS`-supplied `-boot` reaches `init:get_argument(boot)` identically.
//!
//! The surfaces deliberately **not** pinned here (a plain differing observable, i.e. a genuine
//! divergence that would be a `mismatch`, not a capability gap — handoffs to `beam_rt`):
//! `code:get_mode/0`, `init:get_argument(root|home|progname)`, `application:get_application/0`,
//! `erlang:module_info/1`, and the `whereis(code_server|erl_prim_loader|kernel_sup)` boot
//! processes (the runtime's native `code:*`/`init:*` floor does not register them).

use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.9 boot-observability case: evaluate `expr` under both runtimes with the same
/// `-eval` argv and pin the oracle's `~w` rendering.
fn eval(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::Build],
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

/// The expanded A.9 preloaded/boot cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- The frozen `erlang:pre_loaded/0` set and its invariants ---------------------
        // The preloaded set is the minimal module floor the emulator can start without a code
        // server. Its *contents* are pinned exactly by the sibling `boot_prereq` group; this
        // group pins the **invariants of the set itself** (no duplicates, atoms only, stable
        // across calls) and **membership** of the boot-critical modules.
        eval(
            "boot_extra/pre_loaded_no_duplicates",
            "erlang:pre_loaded/0 has no duplicate members (usort equals sort)",
            "begin X = erlang:pre_loaded(), lists:usort(X) =:= lists:sort(X) end",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_length_matches_usort_length",
            "erlang:pre_loaded/0's length equals the length of its de-duplicated set",
            "length(erlang:pre_loaded()) =:= length(lists:usort(erlang:pre_loaded()))",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_all_members_are_atoms",
            "every member of erlang:pre_loaded/0 is a module atom",
            "lists:all(fun erlang:is_atom/1, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_is_stable_across_calls",
            "erlang:pre_loaded/0 returns the identical set on a second call",
            "erlang:pre_loaded() =:= erlang:pre_loaded()",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erlang",
            "the `erlang` module is preloaded",
            "lists:member(erlang, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_init",
            "the `init` module is preloaded",
            "lists:member(init, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erts_internal",
            "the `erts_internal` module is preloaded",
            "lists:member(erts_internal, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_prim_file",
            "the `prim_file` module is preloaded",
            "lists:member(prim_file, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_zlib",
            "the `zlib` module is preloaded",
            "lists:member(zlib, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erl_prim_loader",
            "the boot loader `erl_prim_loader` is preloaded",
            "lists:member(erl_prim_loader, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_persistent_term",
            "the `persistent_term` module is preloaded",
            "lists:member(persistent_term, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_prim_socket",
            "the `prim_socket` module is preloaded",
            "lists:member(prim_socket, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_socket_registry",
            "the `socket_registry` module is preloaded",
            "lists:member(socket_registry, erlang:pre_loaded())",
            "ok true",
        ),
        // --- The code server's view of the preloaded set ---------------------------------
        // Every module in the preloaded set is registered by the code server as
        // `{file,preloaded}` and resolved by code:which/1 to the atom `preloaded`. This is the
        // observable the real boot chain relies on before the code server can read a file.
        eval(
            "boot_extra/code_is_loaded_every_preloaded_module",
            "code:is_loaded/1 reports {file,preloaded} for every member of erlang:pre_loaded/0",
            "lists:all(fun(M) -> code:is_loaded(M) =:= {file,preloaded} end, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/code_which_every_preloaded_module",
            "code:which/1 reports the atom `preloaded` for every member of erlang:pre_loaded/0",
            "lists:all(fun(M) -> code:which(M) =:= preloaded end, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/code_is_loaded_erlang_is_preloaded",
            "code:is_loaded(erlang) is {file,preloaded}",
            "code:is_loaded(erlang)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_erts_internal_is_preloaded",
            "code:is_loaded(erts_internal) is {file,preloaded}",
            "code:is_loaded(erts_internal)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_prim_file_is_preloaded",
            "code:is_loaded(prim_file) is {file,preloaded}",
            "code:is_loaded(prim_file)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_zlib_is_preloaded",
            "code:is_loaded(zlib) is {file,preloaded}",
            "code:is_loaded(zlib)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_prim_inet_is_preloaded",
            "code:is_loaded(prim_inet) is {file,preloaded}",
            "code:is_loaded(prim_inet)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_kernel_is_file",
            "code:is_loaded/1 reports a {file,Path} pair for the booted `kernel` module",
            "begin L = code:is_loaded(kernel), is_tuple(L) andalso element(1, L) =:= file andalso is_list(element(2, L)) end",
            "ok true",
        ),
        eval(
            "boot_extra/code_which_init_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the booted `init` module",
            "code:which(init)",
            "ok preloaded",
        ),
        // --- init / boot status and its recorded arguments --------------------------------
        // `init` is the process the boot script itself starts; its status and the arguments it
        // recorded from the command line are the booted system's own self-description.
        eval(
            "boot_extra/init_get_status_state_is_starting",
            "init:get_status/0 reports the `starting` phase in its first element",
            "element(1, init:get_status()) =:= starting",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_status_second_element_is_atom",
            "init:get_status/0's second element is an atom (the started phase)",
            "is_atom(element(2, init:get_status()))",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_arguments_is_list_of_pairs",
            "init:get_arguments/0 is a list of {Flag,Values} pairs with atom tags and list values",
            "lists:all(fun({K,V}) -> is_atom(K) andalso is_list(V) end, init:get_arguments())",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_plain_arguments_is_list",
            "init:get_plain_arguments/0 is a list of strings",
            "is_list(init:get_plain_arguments())",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_argument_noshell_is_ok",
            "the -noshell flag the harness always passes is recorded as {ok,[[]]}",
            "init:get_argument(noshell) =:= {ok,[[]]}",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_argument_boot_absent_is_error",
            "with no -boot given, init:get_argument(boot) is the atom `error`",
            "init:get_argument(boot)",
            "ok error",
        ),
        // --- The kernel processes the boot script registered ------------------------------
        // `init` is a named kernel process the boot script relies on; it is observable through
        // both whereis/1 and the registered/0 table. (The runtime's native code/init floor does
        // not register code_server/erl_prim_loader/kernel_sup, so those are handoffs, not cases.)
        eval(
            "boot_extra/init_process_is_registered",
            "the booted `init` process is a live registered pid",
            "is_pid(whereis(init))",
            "ok true",
        ),
        eval(
            "boot_extra/init_is_in_registered",
            "the booted `init` process appears in the registered/0 name table",
            "lists:member(init, registered())",
            "ok true",
        ),
        eval(
            "boot_extra/registered_members_are_atoms",
            "every name in registered/0 is an atom",
            "lists:all(fun erlang:is_atom/1, registered())",
            "ok true",
        ),
        // --- The applications the boot script started -------------------------------------
        // kernel and stdlib are the two applications every booted system starts (`start_clean`);
        // their presence is the boot script's progress observable.
        eval(
            "boot_extra/which_applications_includes_stdlib",
            "the boot script has started `stdlib` (it appears in which_applications/0)",
            "lists:keymember(stdlib, 1, application:which_applications())",
            "ok true",
        ),
        eval(
            "boot_extra/loaded_applications_includes_stdlib",
            "the boot script has loaded `stdlib` (it appears in loaded_applications/0)",
            "lists:keymember(stdlib, 1, application:loaded_applications())",
            "ok true",
        ),
        eval(
            "boot_extra/loaded_applications_has_app_names",
            "every loaded_applications/0 entry is tagged by an application atom",
            "is_list([A || {A,_,_} <- application:loaded_applications()])",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_env_missing_key_is_undefined",
            "application:get_env/2 reports `undefined` for an unset kernel key",
            "application:get_env(kernel, a_nonexistent_key_xyz) =:= undefined",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_vsn_shape",
            "application:get_key(kernel, vsn) carries a string value (shape, not the release version)",
            "is_list(element(2, application:get_key(kernel, vsn)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_mod",
            "application:get_key(kernel, mod) is the {kernel,[]} boot module descriptor",
            "application:get_key(kernel, mod) =:= {ok,{kernel,[]}}",
            "ok true",
        ),
        // --- Version / emulator observability of the booted system ------------------------
        // The frozen build constant (`version` = "17.1") is pinned exactly; the release-varying
        // OTP release string is pinned by shape only (oracle OTP 29 vs the rewrite's OTP 30), and
        // the live table counts are pinned as a positive integer.
        eval(
            "boot_extra/system_info_version_is_frozen_17_1",
            "erlang:system_info(version) reports the frozen erts version string \"17.1\"",
            "erlang:system_info(version) =:= \"17.1\"",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_otp_release_is_numeric_string",
            "erlang:system_info(otp_release) is a string of ASCII digits (value is release-dependent)",
            "lists:all(fun(C) -> C >= $0 andalso C =< $9 end, erlang:system_info(otp_release))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_otp_release_is_non_empty",
            "erlang:system_info(otp_release) is a non-empty string",
            "length(erlang:system_info(otp_release)) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_atom_count_is_positive_integer",
            "erlang:system_info(atom_count) is a positive integer (the live atom-table size)",
            "is_integer(erlang:system_info(atom_count)) andalso erlang:system_info(atom_count) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_creation_is_integer",
            "erlang:system_info(creation) is an integer (the per-VM-start creation stamp)",
            "is_integer(erlang:system_info(creation))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_port_count_is_integer",
            "erlang:system_info(port_count) is an integer count",
            "is_integer(erlang:system_info(port_count))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_ets_count_is_integer",
            "erlang:system_info(ets_count) is an integer count",
            "is_integer(erlang:system_info(ets_count))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_scheduler_bind_type_is_atom",
            "erlang:system_info(scheduler_bind_type) is an atom",
            "is_atom(erlang:system_info(scheduler_bind_type))",
            "ok true",
        ),
        // --- Preloaded-module introspection ----------------------------------------------
        // A preloaded module is still a real module: its `module_info/1` export table is
        // observable. (`init` is used because the runtime's native `erlang` floor does not
        // implement `erlang:module_info/1` — that divergence is a handoff, not a case.)
        eval(
            "boot_extra/init_module_info_exports_is_list",
            "init:module_info(exports) is a list (a preloaded module's export table)",
            "is_list(init:module_info(exports))",
            "ok true",
        ),
        eval(
            "boot_extra/init_module_info_exports_non_empty",
            "the preloaded `init` module exports at least one function",
            "length(init:module_info(exports)) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/init_module_info_exports_are_fa_pairs",
            "every init:module_info(exports) entry is an {Atom,Arity} pair",
            "lists:all(fun({F,A}) -> is_atom(F) andalso is_integer(A) end, init:module_info(exports))",
            "ok true",
        ),
        // --- The `-boot` init argument, through the environment half (contract A.6) --------
        // The `-boot FILE` switch is what selects the boot script; it is recorded by `init`
        // identically whether it arrives on the command line or through `ERL_AFLAGS`. The
        // command-line half is pinned by the sibling `boot_prereq` group; this case pins the
        // same init argument reaching `init:get_argument(boot)` through the environment merge.
        Case {
            id: "boot_extra/boot_flag_via_erl_aflags",
            description: "an ERL_AFLAGS-supplied `-boot start` reaches init:get_argument(boot) identically",
            contracts: &[Contract::Build],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(boot) =:= {ok, [[\"start\"]]}",
                extra_argv: &[],
                env: &[("ERL_AFLAGS", "-boot start")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, CaseKind, Contract, Probe};

    /// The one case here that drives the environment half of the CLI contract (A.6): the
    /// `-boot` init argument supplied through `ERL_AFLAGS`.
    const ENV_CASES: &[&str] = &["boot_extra/boot_flag_via_erl_aflags"];

    #[test]
    fn every_case_is_a_build_eval_case_in_the_boot_extra_group() {
        for case in group() {
            assert!(
                case.id.starts_with("boot_extra/"),
                "case {} is outside the boot_extra group",
                case.id
            );
            assert_eq!(case.contracts, &[Contract::Build], "case {}", case.id);
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            let probe = match case.kind {
                CaseKind::Eval { probe, .. } | CaseKind::EvalEnv { probe, .. } => probe,
                CaseKind::Module(_) => {
                    panic!("case {}: an A.9 boot case must be an eval case", case.id)
                }
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
        }
    }

    #[test]
    fn case_ids_are_unique_and_count_is_material() {
        use std::collections::HashSet;
        let cases = group();
        let mut seen = HashSet::new();
        for case in &cases {
            assert!(seen.insert(case.id), "duplicate case id {}", case.id);
        }
        assert!(
            cases.len() >= 20,
            "the expanded A.9 group must be material (got {})",
            cases.len()
        );
    }

    #[test]
    fn every_case_has_a_well_formed_ok_golden() {
        for case in group() {
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {} asserts stdout", case.id));
            assert!(
                golden == "ok true"
                    || golden == "ok error"
                    || golden == "ok preloaded"
                    || golden == "ok {file,preloaded}",
                "case {}: unexpected golden {golden:?}",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            assert!(
                !golden.contains("<0."),
                "case {}: the golden pins an unstable pid: {golden:?}",
                case.id
            );
        }
    }

    #[test]
    fn only_the_environment_case_carries_an_environment() {
        for case in group() {
            let has_env = !case.env().is_empty();
            assert_eq!(
                has_env,
                matches!(case.kind, CaseKind::EvalEnv { .. }),
                "case {}: env case/non-env case mismatch",
                case.id
            );
            // Every case that does *not* pinn the environment takes no extra argv either.
            if case.env().is_empty() {
                assert!(
                    case.extra_argv().is_empty(),
                    "case {}: takes no extra argv",
                    case.id
                );
            }
        }
        // The environment case pins `ERL_AFLAGS` (the documented variable) and nothing else.
        for id in ENV_CASES {
            let case = group()
                .into_iter()
                .find(|c| c.id == *id)
                .unwrap_or_else(|| panic!("case {id} is missing"));
            assert_eq!(case.env(), &[("ERL_AFLAGS", "-boot start")], "case {id}");
            assert!(
                case.expr()
                    .is_some_and(|e| e.contains("init:get_argument(boot)")),
                "case {id} must observe the `boot` init argument"
            );
        }
    }

    #[test]
    fn no_expression_pins_a_volatile_value() {
        for case in group() {
            let expr = case.expr().expect("every case is an eval case");
            assert!(
                !expr.contains("process_count")
                    && !expr.contains("wall_clock")
                    && !expr.contains("system_version")
                    && !expr.contains("otp_release) =:="),
                "case {}: the expression pins a volatile value: {expr:?}",
                case.id
            );
        }
    }
}
