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
//!
//! A check receives the [`Workload`] as well as the result, because a couple of them
//! need the workload's own atom table: a comparison never fabricates a boolean, it
//! returns the module's **own** `true`/`false` atom word, and an atom index only means
//! something inside the table it was interned in.

use beam_term::{is_small, make_atom, signed_value, Eterm};

use super::inlined::{AGGR_RESULT, FIB_RESULT, KERNEL_RESULT, WIDE_RESULT};
use super::workloads::{
    Workload, BUILD_ROUNDS, CROSS_STEPS, MAP_ROUNDS, SELECT_ROUNDS, SUM_LENGTH, WALK_LENGTH,
};
use crate::terms;
/// Assert `result` is the small integer `expected`.
fn expect_small(result: Eterm, expected: i64) -> std::result::Result<(), String> {
    if !is_small(result) || signed_value(result) != expected {
        return Err(format!("expected the small int {expected}, got {result:?}"));
    }
    Ok(())
}

/// `fib/1` must return the 20th Fibonacci number the fixture computes.
pub(crate) fn verify_fib(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, FIB_RESULT)
}

/// The arithmetic kernel's fixed point: both accumulators start at `0`, so every
/// chained `'+'` stays `0`.
pub(crate) fn verify_kernel(
    _workload: &Workload,
    result: Eterm,
) -> std::result::Result<(), String> {
    expect_small(result, KERNEL_RESULT)
}

/// The wide kernel's final register: each round writes `1` into `x(r)` and accumulates
/// it into `x(r + 1)`, so the last round (`r = WIDE_OPS - 1`) leaves `1` in `x(0)`.
pub(crate) fn verify_wide(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, WIDE_RESULT)
}

/// The aggregate kernel multiplies by the constant the reads produce (`1`), so the
/// accumulator stays `1` (a taken fail arm would make it `-1`).
pub(crate) fn verify_aggr(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, AGGR_RESULT)
}

/// The comparison kernel's trailing comparison is `x(2)` (3) `<` `x(1)` (20), so the
/// result must be the module's **own** `true` atom — not a fabricated boolean.
pub(crate) fn verify_cmp(workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    match workload.module.atom_table.lookup("true").map(make_atom) {
        Some(truthy) if result == truthy => Ok(()),
        Some(truthy) => Err(format!(
            "expected the module's `true` atom ({truthy:?}), got {result:?}"
        )),
        None => Err("the comparison module must intern `true`".to_string()),
    }
}

/// `build/0` must return a list of [`BUILD_ROUNDS`] ones.
pub(crate) fn verify_build(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
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
pub(crate) fn verify_map(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, (MAP_ROUNDS * 6) as i64)
}

/// `select/1` must return `SELECT_ROUNDS * 3` (the scrutinee per round).
pub(crate) fn verify_select(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, (SELECT_ROUNDS * 3) as i64)
}

/// `walk/2` must return [`WALK_LENGTH`] (one per cell).
pub(crate) fn verify_walk(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, WALK_LENGTH as i64)
}

/// `sum/1` must return the triangular number `SUM_LENGTH * (SUM_LENGTH + 1) / 2` — cell
/// `i` of the argument holds `i`.
pub(crate) fn verify_sum(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, (SUM_LENGTH * (SUM_LENGTH + 1) / 2) as i64)
}

/// `cross/2` must return [`CROSS_STEPS`] (one per cross-module transfer).
pub(crate) fn verify_cross(_workload: &Workload, result: Eterm) -> std::result::Result<(), String> {
    expect_small(result, CROSS_STEPS as i64)
}