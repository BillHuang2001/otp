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
//! The **converted census roster**: one module per instruction group.
//!
//! Each group file exposes `pub fn rows(atoms: &ProbeAtoms) -> Vec<Probe>` and owns
//! a **disjoint** set of `beam_loader::Instr` variant names (spelled out in the
//! group's doc comment), so a row can be converted, rewritten or debugged without
//! touching another group — or the harness in the parent module.
//!
//! [`all_rows`] is the union the census consumes: `tests/census.rs` keys the
//! result by variant name and lets a probe row **override** the legacy
//! degenerate-operand roster entry of that name, so a group that returns an empty
//! `Vec` (the state of a not-yet-converted group) leaves the legacy rows in place.

pub mod bitstrings;
pub mod core;
pub mod data;
pub mod exceptions;
pub mod receive;

use super::ProbeAtoms;

/// Every converted row, in no particular order (the census keys them by variant
/// name).
pub fn all_rows(atoms: &ProbeAtoms) -> Vec<super::Probe> {
    let mut rows = Vec::new();
    rows.extend(core::rows(atoms));
    rows.extend(data::rows(atoms));
    rows.extend(exceptions::rows(atoms));
    rows.extend(receive::rows(atoms));
    rows.extend(bitstrings::rows(atoms));
    rows
}
