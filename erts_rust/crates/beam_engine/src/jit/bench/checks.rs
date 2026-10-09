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

//! Per-workload result checks.
//!
//! Each workload in [`super::workloads`] pins the value its body is *supposed* to
//! compute. The check is what makes "the tiers agree" meaningful: two tiers agreeing on
//! a **wrong** answer still fails here, so a workload can never degenerate into a
//! tautology that only compares a value against itself.

use beam_term::{is_small, signed_value, Eterm};

use super::workloads::{BUILD_ROUNDS, MAP_ROUNDS, SELECT_ROUNDS, WALK_LENGTH};
use crate::terms;

/// Assert `result` is the small integer `expected`.
fn expect_small(result: Eterm, expected: i64) -> std::result::Result<(), String> {
    if !is_small(result) || signed_value(result) != expected {
        return Err(format!("expected the small int {expected}, got {result:?}"));
    }
    Ok(())
}

/// `build/0` must return a list of [`BUILD_ROUNDS`] ones.
pub(super) fn verify_build(result: Eterm) -> std::result::Result<(), String> {
    let items = terms::list_elements(result).map_err(|e| format!("not a proper list: {e:?}"))?;
    if items.len() != BUILD_ROUNDS {
        return Err(format!(
            "expected {BUILD_ROUNDS} list cells, got {}",
            items.len()
        ));
    }
    if let Some(bad) = items
        .iter()
        .find(|item| !is_small(**item) || signed_value(**item) != 1)
    {
        return Err(format!("every list element must be 1, found {bad:?}"));
    }
    Ok(())
}

/// `map/2` must return `MAP_ROUNDS * 6` (1 + 2 + 3 per round).
pub(super) fn verify_map(result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, (MAP_ROUNDS * 6) as i64)
}

/// `select/1` must return `SELECT_ROUNDS * 3` (the scrutinee per round).
pub(super) fn verify_select(result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, (SELECT_ROUNDS * 3) as i64)
}

/// `walk/2` must return [`WALK_LENGTH`] (one per cell).
pub(super) fn verify_walk(result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, WALK_LENGTH as i64)
}
