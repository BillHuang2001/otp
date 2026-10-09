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

//! Machine-readable reports: a stable JSON schema plus a human rendering.
//!
//! The JSON schema is versioned by the `schema` field ([`SCHEMA`]) and only ever grows.
//! An observable that does not apply is `null` (`stdout`/`stdout_hex`/`exit_code` for a
//! side that did not run, `matched_golden` where no golden was asserted), and the two
//! optional keys are omitted rather than guessed: `reason` only appears for an
//! `unsupported` side and `matched_golden` only where a golden was actually checked.
//! Reports are written under the crate's gitignored `out/` directory unless `--out <dir>`
//! overrides it.
//!
//! The module is split by responsibility: [`model`] holds the serde structs and their
//! constructors, [`json`] the schema id and the report writers, and [`render`] the
//! human-readable rendering. Every path that used to be `report::<Name>` is re-exported
//! here, so the public API is unchanged.

mod json;
mod model;
mod render;

#[cfg(test)]
mod test_support;

pub use json::{
    default_out_dir, to_json_string, write_json, write_json_value, LIST_SCHEMA, SCHEMA,
};
pub use model::{
    CaseReport, ContractEntry, ContractGroup, ListEntry, ListReport, RunReport, RuntimeReport,
    SideReport, Summary,
};
pub use render::{render_human, render_list_grouped};
