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

//! Contract **A.4** — the port-*opening* grammar and its error shapes.
//!
//! Split out of [`super::driver`] by responsibility: that group drives a working port
//! (`os:cmd/1`, the read path, `port_command/2,3` round trips, `port_info/1,2`); this one
//! pins what `open_port/2` does with the arguments it must **reject** (an empty or non-string
//! name, a name that is not a tuple, an unknown option) or with a `spawn_executable` child
//! that cannot be started.
//!
//! Like every port case these open real (or deliberately unopenable) OS processes, so they
//! stay small and fast. An empty name is validated *before* any driver runs, so it never
//! spawns anything. Every golden below is the **oracle's** own output, re-measured with
//! `differential run <id>` (never a hand-written guess).
//!
//! # Measured divergences that are deliberately NOT cases (handoffs, never `mismatch`-pinned)
//!
//! Each item below is a plain Erlang value/reason, so pinning it would be a `mismatch`; it is
//! reported to the runtime owners (`beam_rt` / `beam_io`) instead. Verified in this
//! environment (oracle OTP 29.1.1 / `erts-17.1`; the built `beam_rt`) with `differential
//! compare`:
//!
//! * **the `{fd, In, Out}` adopted-descriptor form.** The oracle's `open_port/2` requires the
//!   **three**-element `{fd, In, Out}` tuple — its C `open_port` answers `badarg` for the
//!   two-element `{fd, N}` — while `beam_rt` accepts the two-element `{fd, N}` and answers
//!   `badarg` for the three-element form. So the two runtimes accept *disjoint* shapes:
//!   `open_port({fd, 1, 1}, [out])` opens under the oracle (a following `port_command/2`
//!   really writes to the process stdout — `"hellook <<…>>"`) but is `error:badarg` under
//!   `beam_rt`; `open_port({fd, 1}, [out])` is `error:badarg` under the oracle but opens under
//!   `beam_rt` (`"hellook <<…>>"`). Neither fd grammar can be pinned until they agree.
//! * **the `{line, N}` option.** With a child that emits a line the oracle frames its input as
//!   `{Port, {data, {eol, Line}}}` (`Line` without its terminator); `beam_rt` delivers the raw
//!   bytes `{Port, {data, <<"hello\n">>}}` — no line framing.
//! * **the `eof` option.** `open_port({spawn, "true"}, [binary, eof])` delivers `{Port, eof}`
//!   under the oracle once the child closes its output; `beam_rt`'s `eof` is inert.
//! * **`port_command(P, 42)`** (a non-iodata payload): the oracle raises `error:badarg`;
//!   `beam_rt` accepts it and answers `true`.
//! * **`port_info(P, registered_name)`**: the oracle answers `[]`; `beam_rt` raises
//!   `error:badarg`.
//!
//! The structural half of A.4 (that a C translation unit referencing the `driver_*` /
//! `erl_drv_*` declarations still compiles against the frozen headers) is asserted in
//! `tests/abi.rs`.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.4 port-open eval case.
///
/// The expression wraps the `open_port/2` call in `try … catch error:Reason -> Reason end`
/// so the *reason atom* (not a volatile stack trace) is the observable, and `Probe::Value`
/// prints it as a readable line (`ok badarg` / `ok enoent`).
fn case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::DriverAbi],
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

/// The A.4 port-open / error-shape cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "driver/open_port_empty_name_badarg",
            "open_port/2 rejects an empty {spawn, \"\"} name with badarg (validated before any driver)",
            "try open_port({spawn,\"\"},[]) catch error:Reason -> Reason end",
            "ok badarg",
        ),
        case(
            "driver/open_port_empty_executable_name_badarg",
            "open_port/2 rejects an empty {spawn_executable, \"\"} name with badarg",
            "try open_port({spawn_executable,\"\"},[]) catch error:Reason -> Reason end",
            "ok badarg",
        ),
        case(
            "driver/spawn_executable_missing_path_is_enoent",
            "a spawn_executable child that cannot be started raises error:enoent (not an I/O diagnostic)",
            "try open_port({spawn_executable,\"/nonexistent/definitely/missing/nope\"},[]) catch error:Reason -> Reason end",
            "ok enoent",
        ),
        case(
            "driver/open_port_non_string_name_badarg",
            "open_port/2 rejects a non-string {spawn, 42} name with badarg",
            "try open_port({spawn,42},[]) catch error:Reason -> Reason end",
            "ok badarg",
        ),
        case(
            "driver/open_port_non_tuple_name_badarg",
            "open_port/2 rejects a name that is not a tuple with badarg",
            "try open_port(42,[]) catch error:Reason -> Reason end",
            "ok badarg",
        ),
        case(
            "driver/open_port_unknown_option_is_badarg",
            "open_port/2 rejects an unknown option with badarg (before any child is spawned)",
            "try open_port({spawn,\"cat\"},[nope]) catch error:Reason -> Reason end",
            "ok badarg",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.4 port-open grammar group: every case is an eval case claiming
    //! [`Contract::DriverAbi`], its expression returns the *reason atom* through a `try …
    //! catch`, its golden is that reason as the oracle prints it, and it never binds a
    //! probe-wrapper variable.

    use super::{group, CaseKind, Contract, Probe};
    use std::collections::HashSet;

    /// Identifier-ish tokens of an Erlang expression (`Reason` is one token, `=:=` is two).
    fn identifiers(expr: &str) -> Vec<&str> {
        expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '@'))
            .filter(|t| !t.is_empty())
            .collect()
    }

    #[test]
    fn every_case_is_a_well_formed_driver_eval_case() {
        let mut ids = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("driver/"),
                "case {} is outside the A.4 driver group",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            assert!(ids.insert(case.id), "case {} is registered twice", case.id);
            assert_eq!(
                case.contracts,
                &[Contract::DriverAbi],
                "case {}: a port-open case evidences the driver ABI (A.4)",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            // A `try … catch` hands the *reason atom* back as the value, so the golden is the
            // oracle's `ok <reason>` line — never an `exception …` line (which would embed a
            // volatile stack trace).
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {} has no golden stdout", case.id));
            assert!(
                matches!(golden, "ok badarg" | "ok enoent"),
                "case {}: unexpected port-open reason golden {golden:?}",
                case.id
            );
            assert!(
                !golden.ends_with('\n'),
                "case {}: the golden is a line, without the trailing newline",
                case.id
            );
            let CaseKind::Eval {
                expr,
                extra_argv,
                probe,
            } = case.kind
            else {
                panic!("case {}: an A.4 port-open case must be an eval case", case.id);
            };
            assert_eq!(probe, Probe::Value, "case {}: probe", case.id);
            assert!(extra_argv.is_empty(), "case {}: no extra argv", case.id);
            assert!(
                expr.starts_with("try ") && expr.ends_with("end"),
                "case {}: the expression must return the caught reason, got {expr:?}",
                case.id
            );
            // The probe wrapper binds `V`/`C`/`R`; an expression re-using one of them makes
            // the oracle's `erl_eval` fail with `unsafe_var` instead of evaluating.
            let tokens = identifiers(expr);
            for reserved in ["V", "C", "R"] {
                assert!(
                    !tokens.contains(&reserved),
                    "case {}: expression must not bind `{reserved}` (probe-wrapper collision)",
                    case.id
                );
            }
        }
    }

    #[test]
    fn one_missing_executable_case_is_the_enoent_shape() {
        let case = group()
            .into_iter()
            .find(|c| c.id == "driver/spawn_executable_missing_path_is_enoent")
            .expect("the enoent case is present");
        assert_eq!(case.golden.stdout, Some("ok enoent"));
        // The path must name nothing, so the failing spawn (not a successful one) is what the
        // oracle reports; a re-measurement that made the path exist would drift the golden.
        let CaseKind::Eval { expr, .. } = case.kind else {
            panic!("expected an eval case");
        };
        assert!(
            expr.contains("spawn_executable") && expr.contains("/nonexistent/"),
            "the enoent case must open a spawn_executable on a missing absolute path, got {expr:?}"
        );
    }
}
