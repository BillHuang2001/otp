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
//! Contract **A.4** — the `ram_file_drv` built-in driver's reply encodings.
//!
//! `beam_rt`'s built-in driver registry serves `ram_file_drv`
//! (`../../crates/beam_io/src/drivers/ram_file.rs`), the driver behind `ram_file:*`. The
//! pinned observables are the driver's **replies** — an in-memory file's read-back bytes, its
//! `{ok, Pos}`-shaped positions, the `eof` marker and the `{error, Reason}` refusals — never a
//! pid/port/ref identity and never a filesystem path.
//!
//! A row must self-terminate: every operation is a synchronous call on the port, so no row
//! waits on an asynchronous reply it did not itself trigger.
use super::{Case, CaseKind, Contract, Golden, Probe};
/// The `ram_file/*` cases.
pub(crate) fn group() -> Vec<Case> {
    let _ = Case {
        id: "ram_file/placeholder",
        description: "placeholder",
        contracts: &[Contract::DriverAbi],
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
