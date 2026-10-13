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

//! Contract **A.7** — the `erl_ddll` dynamic-driver-loader surface.
//!
//! `erl_ddll` is the OTP-application face of the emulator's dynamic driver loader
//! (`load/2`, `try_load/3`, `unload/1`, `try_unload/2`, `loaded_drivers/0`, `info/2`). What a
//! program can depend on — and what this group pins — is each call's **result shape** and its
//! **error reasons**, never the *contents* of the loaded-driver list: those are
//! installation-dependent (the oracle's statically linked `tcp_inet`, `udp_inet` and
//! `ram_file_drv` versus the Rust runtime's empty list), so only order-stable projections of the
//! list are asserted.
//!
//! * `loaded_drivers/0` — pinned as `{ok, List}` with `List` a list of strings, via the tuple
//!   shape, `is_list/1`, an all-strings fold and a filtered projection that is empty on both
//!   runtimes. The list's members are deliberately **not** pinned.
//! * `try_load/3` — a missing object, a missing directory and a non-string name all answer
//!   `{error,{open_error,-10}}` on both runtimes; an unknown option and a non-string directory
//!   are `error:badarg`.
//! * `try_unload/2` — a driver that is not loaded answers `{error,not_loaded}`; an unknown
//!   option is `error:badarg`.
//! * `info/2` — every probed form (a statically linked driver, an unknown name, an unknown key,
//!   a non-string name) raises `error:badarg` on both runtimes.
//!
//! Two shapes **diverge** between the two runtimes and are therefore deliberately absent:
//! `try_load(".", "tcp_inet", [])` and `try_unload("tcp_inet", [])` answer
//! `{error,linked_in_driver}` on the oracle but the generic missing-object answer on the Rust
//! runtime. They are recorded in `divergences.md`.
//!
//! Every case is an [`CaseKind::Eval`] under the default `Probe::TermToBinary`, so its golden is
//! the oracle's exact stdout line plus exit code.

use super::{Case, CaseKind, Contract, Golden, Probe};

pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- loaded_drivers/0: the shape of the result, never the installation's driver list ---
        Case {
            id: "ddll/loaded_drivers_answers_ok",
            description: "erl_ddll:loaded_drivers/0 answers ok",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "element(1, erl_ddll:loaded_drivers())",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,111,107>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "ddll/loaded_drivers_result_is_a_two_element_tuple",
            description: "erl_ddll:loaded_drivers/0 returns a 2-tuple",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "tuple_size(erl_ddll:loaded_drivers())",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "ddll/loaded_drivers_second_element_is_a_list",
            description: "the driver list of erl_ddll:loaded_drivers/0 is a list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "is_list(element(2, erl_ddll:loaded_drivers()))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "ddll/loaded_driver_names_are_strings",
            description: "every entry of the loaded-driver list is a string",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:all(fun erlang:is_list/1, element(2, erl_ddll:loaded_drivers()))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "ddll/loaded_drivers_projection_matches_no_unknown_driver",
            description: "a filtered projection over the loaded-driver list is empty for a name that is never loaded",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "[D || D <- element(2, erl_ddll:loaded_drivers()), D =:= \"nosuch_driver_xyz\"]",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,106>>"),
                exit_code: 0,
            },
        },
        // --- try_load/3: the missing-object / missing-directory shapes -------------------------
        Case {
            id: "ddll/try_load_missing_object_at_cwd",
            description: "erl_ddll:try_load/3 of a missing object is {error,{open_error,-10}}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:try_load(\".\", \"nosuch\", [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,5,101,114,114,111,114,104,2,119,10,111,112,101,110,95,101,114,114,111,114,98,255,255,255,246>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "ddll/try_load_missing_directory",
            description: "erl_ddll:try_load/3 in a missing directory is {error,{open_error,-10}}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:try_load(\"/nonexistent_dir_xyz\", \"nosuch\", [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,5,101,114,114,111,114,104,2,119,10,111,112,101,110,95,101,114,114,111,114,98,255,255,255,246>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "ddll/try_load_non_string_name",
            description: "erl_ddll:try_load/3 with a non-string name answers {error,{open_error,-10}}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:try_load(\".\", nosuch_atom, [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,5,101,114,114,111,114,104,2,119,10,111,112,101,110,95,101,114,114,111,114,98,255,255,255,246>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "ddll/try_load_unknown_option_is_badarg",
            description: "erl_ddll:try_load/3 with an unknown option is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:try_load(\".\", \"nosuch\", [bogus])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "ddll/try_load_non_string_directory_is_badarg",
            description: "erl_ddll:try_load/3 with a non-string directory is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:try_load(123, \"foo\", [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- try_unload/2: the not-loaded shape -----------------------------------------------
        Case {
            id: "ddll/try_unload_a_not_loaded_driver",
            description: "erl_ddll:try_unload/2 of a driver that is not loaded is {error,not_loaded}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:try_unload(\"nosuch\", [])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,5,101,114,114,111,114,119,10,110,111,116,95,108,111,97,100,101,100>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "ddll/try_unload_unknown_option_is_badarg",
            description: "erl_ddll:try_unload/2 with an unknown option is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:try_unload(\"nosuch\", [bogus])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- info/2: the error:badarg refusal shapes ------------------------------------------
        Case {
            id: "ddll/info_of_a_builtin_driver_is_badarg",
            description: "erl_ddll:info/2 of a statically linked driver is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:info(\"tcp_inet\", loaded)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "ddll/info_of_an_unknown_name_is_badarg",
            description: "erl_ddll:info/2 of an unknown driver name is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:info(\"nosuch\", loaded)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "ddll/info_of_an_unknown_key_is_badarg",
            description: "erl_ddll:info/2 with an unknown key is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:info(\"tcp_inet\", bogus)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "ddll/info_of_a_non_string_name_is_badarg",
            description: "erl_ddll:info/2 with a non-string name is error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erl_ddll:info(123, loaded)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, Contract, Probe};

    #[test]
    fn the_group_pins_the_loaded_drivers_try_load_try_unload_and_info_surface() {
        let cases = group();
        assert_eq!(cases.len(), 16, "the A.7 erl_ddll group has sixteen cases");
        assert_eq!(
            cases.iter().map(|case| case.id).collect::<Vec<_>>(),
            [
                "ddll/loaded_drivers_answers_ok",
                "ddll/loaded_drivers_result_is_a_two_element_tuple",
                "ddll/loaded_drivers_second_element_is_a_list",
                "ddll/loaded_driver_names_are_strings",
                "ddll/loaded_drivers_projection_matches_no_unknown_driver",
                "ddll/try_load_missing_object_at_cwd",
                "ddll/try_load_missing_directory",
                "ddll/try_load_non_string_name",
                "ddll/try_load_unknown_option_is_badarg",
                "ddll/try_load_non_string_directory_is_badarg",
                "ddll/try_unload_a_not_loaded_driver",
                "ddll/try_unload_unknown_option_is_badarg",
                "ddll/info_of_a_builtin_driver_is_badarg",
                "ddll/info_of_an_unknown_name_is_badarg",
                "ddll/info_of_an_unknown_key_is_badarg",
                "ddll/info_of_a_non_string_name_is_badarg",
            ]
        );
        for case in &cases {
            assert!(case.id.starts_with("ddll/"), "{}", case.id);
            assert_eq!(case.contracts, &[Contract::OtpApps], "{}", case.id);
            assert!(
                case.expr().is_some_and(|expr| expr.contains("erl_ddll:")),
                "{}",
                case.id
            );
            assert!(case.extra_argv().is_empty(), "{}", case.id);
            assert!(case.env().is_empty(), "{}", case.id);
            assert_eq!(case.kind_label(), Probe::TermToBinary.as_str(), "{}", case.id);
            let CaseKind::Eval { probe, .. } = &case.kind else {
                panic!("{} must be an Eval case", case.id);
            };
            assert_eq!(*probe, Probe::TermToBinary, "{}", case.id);
        }
    }

    #[test]
    fn the_loaded_drivers_cases_pin_a_shape_not_the_installation_driver_list() {
        // The loaded-driver *list* is installation-dependent (the oracle's built-in
        // tcp_inet/udp_inet/ram_file_drv versus the runtime's empty list), so every
        // loaded_drivers case reduces the result to an order-stable shape. These five
        // goldens are exactly the encodings of `ok`, `2`, `true`, `true` and `[]`.
        for (id, stdout) in [
            ("ddll/loaded_drivers_answers_ok", "ok <<131,119,2,111,107>>"),
            (
                "ddll/loaded_drivers_result_is_a_two_element_tuple",
                "ok <<131,97,2>>",
            ),
            (
                "ddll/loaded_drivers_second_element_is_a_list",
                "ok <<131,119,4,116,114,117,101>>",
            ),
            (
                "ddll/loaded_driver_names_are_strings",
                "ok <<131,119,4,116,114,117,101>>",
            ),
            (
                "ddll/loaded_drivers_projection_matches_no_unknown_driver",
                "ok <<131,106>>",
            ),
        ] {
            let case = group().into_iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.golden.stdout, Some(stdout), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
        // No installation driver name may leak into any golden.
        for case in group() {
            if let Some(golden) = case.golden.stdout {
                for name in ["tcp_inet", "udp_inet", "ram_file_drv"] {
                    assert!(
                        !golden.contains(name),
                        "case {}: the installation driver name {name:?} leaked into the golden",
                        case.id
                    );
                }
            }
        }
    }

    #[test]
    fn the_documented_open_error_not_loaded_and_badarg_reasons_are_pinned() {
        // Every try_load case that reaches the loader answers the same documented
        // `{error,{open_error,-10}}` tuple; the not-loaded unload is `{error,not_loaded}`.
        const OPEN_ERROR: &str = "ok <<131,104,2,119,5,101,114,114,111,114,104,2,119,10,111,112,101,110,95,101,114,114,111,114,98,255,255,255,246>>";
        const NOT_LOADED: &str = "ok <<131,104,2,119,5,101,114,114,111,114,119,10,110,111,116,95,108,111,97,100,101,100>>";
        for id in [
            "ddll/try_load_missing_object_at_cwd",
            "ddll/try_load_missing_directory",
            "ddll/try_load_non_string_name",
        ] {
            let case = group().into_iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.golden.stdout, Some(OPEN_ERROR), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
        let unload = group()
            .into_iter()
            .find(|c| c.id == "ddll/try_unload_a_not_loaded_driver")
            .expect("registered");
        assert_eq!(unload.golden.stdout, Some(NOT_LOADED));

        // The refusal surface: seven calls raise `error:badarg` and exit 1.
        let badarg: Vec<&str> = group()
            .iter()
            .filter(|case| case.golden.stdout == Some("exception error badarg"))
            .map(|case| case.id)
            .collect();
        assert_eq!(
            badarg,
            [
                "ddll/try_load_unknown_option_is_badarg",
                "ddll/try_load_non_string_directory_is_badarg",
                "ddll/try_unload_unknown_option_is_badarg",
                "ddll/info_of_a_builtin_driver_is_badarg",
                "ddll/info_of_an_unknown_name_is_badarg",
                "ddll/info_of_an_unknown_key_is_badarg",
                "ddll/info_of_a_non_string_name_is_badarg",
            ]
        );
        for case in group() {
            if case.golden.stdout == Some("exception error badarg") {
                assert_eq!(case.golden.exit_code, 1, "{}", case.id);
            }
            // The diverging `linked_in_driver` shapes (a built-in driver through try_load /
            // try_unload) must stay unpinned — only `info/2` may name a built-in driver.
            let expr = case.expr().expect("eval case");
            assert!(
                !(expr.contains("try_load") && expr.contains("tcp_inet"))
                    && !(expr.contains("try_unload") && expr.contains("tcp_inet")),
                "case {}: the diverging linked_in_driver shape must not be pinned",
                case.id
            );
        }
    }
}
