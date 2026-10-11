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

//! Contract **A.6** — CLI flags and configuration, part *application*.
//!
//! [`super::flags`] pins the *recording* half of A.6: which switches the merged argv exposes
//! through `init:get_argument/1` and `erlang:system_info/1`. These cases pin the other
//! half — that a flag an `ERL_*FLAGS` variable carries is actually **applied** by the
//! runtime, not merely recorded:
//!
//! * `flags_apply/erl_*_plus_*` are [`CaseKind::EvalEnv`](super::CaseKind::EvalEnv) cases
//!   whose golden is the *effect* of the switch: `+S N` observed through
//!   `erlang:system_info(schedulers)`, `+A N` through `…(thread_pool_size)`, `+Q N`
//!   through `…(port_limit)`, and `-pa`/`-pz` through `code:get_path/0`.
//! * The `-pa`/`-pz` ordering cases pin the *prepend-vs-append* semantics: `-pa D` puts `D`
//!   at the front of the code path, `-pz D` at the back, so the two orderings of the same
//!   two directories render as each other's reverse. Both directories are the CWD-relative
//!   `..` and `../..`, which is neither runtime's default path (so no absolute path ever
//!   reaches a golden) and which both runtimes store verbatim (the same process working
//!   directory is shared by the two spawns). A runtime that filtered them out for not
//!   existing, or that stored them absolutely, would render an empty list — the case is
//!   self-consistent for both and the golden is the oracle's own output.
//! * `flags_apply/erl_aflags_pa_puts_the_compiled_dir_on_the_code_path` is a
//!   [`CaseKind::ModuleEnv`](super::CaseKind::ModuleEnv) case: the compiled directory is
//!   reachable **only** through `ERL_AFLAGS=-pa {dir}` (the oracle argv carries no `-pa`),
//!   and the executed module calls into a **helper** module that lives only in that
//!   directory. A runtime that recorded the `-pa` without applying it could not resolve the
//!   helper, so the case genuinely discriminates on *application*.
//!
//! Every golden below is the oracle's own output, verified with `differential run <id>`.
//! The probe wrapper binds `V`/`C`/`R`, so no expression here binds those names.
use super::{
    Case, CaseKind, Contract, EmbeddedModule, Golden, ModuleRun, Probe,
};

/// The A.6 *application* cases: an `ERL_*FLAGS`-carried switch observed through its effect.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "flags_apply/erl_aflags_plus_s_sets_schedulers",
            description: "an ERL_AFLAGS-carried +S N is applied to the scheduler count",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "erlang:system_info(schedulers)",
                extra_argv: &[],
                env: &[("ERL_AFLAGS", "+S 3")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags_apply/erl_flags_plus_s_sets_schedulers",
            description: "an ERL_FLAGS-carried +S N is applied to the scheduler count",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "erlang:system_info(schedulers)",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "+S 2")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags_apply/erl_flags_plus_a_sets_thread_pool_size",
            description: "an ERL_FLAGS-carried +A N is applied to the async thread pool size",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "erlang:system_info(thread_pool_size)",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "+A 12")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 12"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags_apply/erl_aflags_plus_a_sets_thread_pool_size",
            description: "an ERL_AFLAGS-carried +A N is applied to the async thread pool size",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "erlang:system_info(thread_pool_size)",
                extra_argv: &[],
                env: &[("ERL_AFLAGS", "+A 12")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 12"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags_apply/erl_flags_plus_q_sets_port_limit",
            description: "an ERL_FLAGS-carried +Q N is applied to the port-table limit",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "erlang:system_info(port_limit)",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "+Q 4096")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 4096"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags_apply/erl_flags_pa_puts_the_dir_on_the_code_path",
            description: "an ERL_FLAGS-carried -pa D really adds D to the code path",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "lists:member(\"..\", code:get_path())",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "-pa ..")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags_apply/erl_flag_pa_prepends_and_pz_appends",
            description: "in ERL_FLAGS, -pa puts its dir before -pz's dir on the code path",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "[case X of \"..\" -> dotdot; \"../..\" -> dotdotdot end || X <- code:get_path(), lists:member(X, [\"..\",\"../..\"])]",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "-pa .. -pz ../..")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [dotdot,dotdotdot]"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags_apply/erl_flag_pz_appends_and_pa_prepends",
            description: "the mirrored -pa/-pz ordering renders the reverse of the other case",
            contracts: &[Contract::Cli],
            kind: CaseKind::EvalEnv {
                expr: "[case X of \"..\" -> dotdot; \"../..\" -> dotdotdot end || X <- code:get_path(), lists:member(X, [\"..\",\"../..\"])]",
                extra_argv: &[],
                env: &[("ERL_FLAGS", "-pa ../.. -pz ..")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [dotdotdot,dotdot]"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags_apply/erl_aflags_pa_puts_the_compiled_dir_on_the_code_path",
            description: "the compiled dir reaches the code path only via ERL_AFLAGS=-pa, so the \
                          module's cross-module call resolves only if the flag was applied",
            contracts: &[Contract::Cli],
            kind: CaseKind::ModuleEnv {
                run: ModuleRun {
                    source: "-module(flags_apply_main).\n\
                             -export([go/0]).\n\
                             go() -> 1 + flags_apply_helper:val().\n",
                    module: "flags_apply_main",
                    function: "go",
                    arity: 0,
                    args: &[],
                    nif: None,
                },
                extra: &[EmbeddedModule {
                    module: "flags_apply_helper",
                    source: "-module(flags_apply_helper).\n\
                             -export([val/0]).\n\
                             val() -> 41.\n",
                }],
                // The oracle argv carries **no** `-pa`; the compiled dir is reachable only
                // through this environment, whose `{dir}` placeholder the harness expands
                // once the module is compiled.
                env: &[("ERL_AFLAGS", "-pa {dir}")],
            },
            golden: Golden {
                stdout: Some("42"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{expand_env, DIR_PLACEHOLDER};

    /// Every case claims A.6, uses a `value` probe for the expression cases, carries a
    /// non-empty environment, and is grouped under the `flags_apply/` id prefix.
    #[test]
    fn the_group_pins_flag_application_under_the_expected_shape() {
        for case in group() {
            assert!(
                case.id.starts_with("flags_apply/"),
                "case {}: unexpected id prefix",
                case.id
            );
            assert_eq!(case.contracts, &[Contract::Cli], "case {}", case.id);
            assert!(
                !case.env().is_empty(),
                "case {}: a flag-application case must set an environment",
                case.id
            );
            for (name, _) in case.env() {
                assert!(
                    name.starts_with("ERL_"),
                    "case {}: {} is not an ERL_* variable",
                    case.id,
                    name
                );
            }
            match &case.kind {
                CaseKind::EvalEnv { probe, .. } => {
                    assert_eq!(*probe, Probe::Value, "case {}", case.id);
                    assert_eq!(case.extra_argv(), &[] as &[&str], "case {}", case.id);
                }
                CaseKind::ModuleEnv { run, extra, env } => {
                    assert_eq!(case.kind_label(), "module-run", "case {}", case.id);
                    assert_eq!(case.expr(), None, "case {}", case.id);
                    assert_eq!(case.extra_argv(), &[] as &[&str], "case {}", case.id);
                    assert_eq!(case.env(), *env, "case {}", case.id);
                    assert!(
                        !extra.is_empty(),
                        "case {}: a discriminating ModuleEnv case needs a helper module",
                        case.id
                    );
                    assert!(
                        env.iter()
                            .any(|(_, value)| value.contains(DIR_PLACEHOLDER)),
                        "case {}: the env must place the compiled dir on the code path",
                        case.id
                    );
                    // The helper can only be found once the placeholder is expanded to the
                    // real compiled dir.
                    let expanded = expand_env(env, std::path::Path::new("/tmp/compiled"));
                    assert!(
                        expanded.iter().any(|(_, value)| value.contains("/tmp/compiled")),
                        "case {}: the placeholder must expand to the compiled dir",
                        case.id
                    );
                    assert!(!run.source.is_empty() && run.source.contains("-module("));
                }
                other => panic!("case {}: unexpected kind {other:?}", case.id),
            }
            assert!(
                case.golden.stdout.is_some(),
                "case {}: a flag-application case asserts an oracle golden",
                case.id
            );
        }
    }
}
