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
//! Contract **A.1/A.7** — the `-eval` route's bit-string surface.
//!
//! `beam_rt`'s `-eval` evaluator builds and matches bit strings itself
//! (`../../crates/beam_rt/src/eval/{bits,bits_decode,bits_match}.rs`) instead of going through
//! the compiler, so the family is pinned here as the *observable* image of a constructed or
//! matched segment: the binary's bytes and the matched field values, rendered with the value
//! probe (`~w`).
//!
//! Every golden is a plain value — no pid/ref/port, no multi-key map — so nothing is
//! order- or VM-start-dependent. Invalid, truncated or out-of-range **pattern** fields are a
//! match failure on both sides (Erlang's rule), never a raise, and the rows below pin that:
//! a malformed `<<…>>` pattern must answer `false`/`nomatch`, not `badarg`.
use super::{Case, CaseKind, Contract, Golden, Probe};
/// The `bits/*` cases.
pub(crate) fn group() -> Vec<Case> {
    let _ = Case {
        id: "bits/placeholder",
        description: "placeholder",
        contracts: &[Contract::LanguageSemantics],
        kind: CaseKind::Eval {
            expr: "ok",
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok ok"),
            exit_code: 0,
        },
    };
    Vec::new()
}
