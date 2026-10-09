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

//! **Test-only** convenience wrappers over the rooted builders.
//!
//! The production **rooted** builders ([`crate::terms::make_tuple_rooted`], …) take
//! the `Machine` and a reader closure so a moving collector can relocate operands
//! between the reserve and the read ([`super::slack`]). The crate's test suite,
//! however, overwhelmingly builds terms from already-materialized `Eterm`s against a
//! non-collecting [`MinimalContext`](crate::context::MinimalContext), where the
//! closure machinery is pure noise.
//!
//! These wrappers keep that ergonomics: they take the pre-built operands, run the
//! same rooted builder with a throwaway `Machine` (whose registers carry no operand
//! — the terms are supplied directly) and a `read` closure that just returns them.
//! They are **not moving-collector-safe** (an already-materialized `Eterm` passed by
//! value is not a root), which is exactly why they exist only under `#[cfg(test)]`
//! and are never reachable from production code.
use beam_common::Result;
use beam_loader::Module;
use beam_term::Eterm;

use crate::context::{ExecutionContext, RootSet};
use crate::machine::Machine;

/// A throwaway machine with no operands bound — the root set the wrappers hand the
/// builder (the terms they build are supplied directly, never through a register).
fn scratch() -> Machine {
    Machine::new()
}

/// Test wrapper for [`crate::terms::make_tuple_rooted`].
pub(crate) fn make_tuple(ctx: &mut dyn ExecutionContext, elems: &[Eterm]) -> Result<Eterm> {
    crate::terms::make_tuple_rooted(
        &mut scratch(),
        ctx,
        RootSet::ALL_LIVE,
        elems.len(),
        0,
        |_, _| Ok(elems.to_vec()),
    )
}

/// Test wrapper for [`crate::terms::make_cons_rooted`].
pub(crate) fn make_cons(
    ctx: &mut dyn ExecutionContext,
    head: Eterm,
    tail: Eterm,
) -> Result<Eterm> {
    crate::terms::make_cons_rooted(&mut scratch(), ctx, RootSet::ALL_LIVE, 0, |_, _| {
        Ok((head, tail))
    })
}

/// Test wrapper for [`crate::terms::make_list_rooted`].
pub(crate) fn make_list(ctx: &mut dyn ExecutionContext, elems: &[Eterm]) -> Result<Eterm> {
    crate::terms::make_list_rooted(
        &mut scratch(),
        ctx,
        RootSet::ALL_LIVE,
        elems.len(),
        0,
        |_, _| Ok(elems.to_vec()),
    )
}

/// Test wrapper for [`crate::terms::make_fun_rooted`].
pub(crate) fn make_fun(
    ctx: &mut dyn ExecutionContext,
    arity: u32,
    uniq: u64,
    index: u32,
    free: &[Eterm],
) -> Result<Eterm> {
    crate::terms::make_fun_rooted(
        &mut scratch(),
        ctx,
        RootSet::ALL_LIVE,
        arity,
        uniq,
        index,
        free.len(),
        0,
        |_, _| Ok(free.to_vec()),
    )
}

/// Test wrapper for [`crate::terms::make_flatmap_rooted`].
pub(crate) fn make_flatmap(ctx: &mut dyn ExecutionContext, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    crate::terms::make_flatmap_rooted(
        &mut scratch(),
        ctx,
        RootSet::ALL_LIVE,
        pairs.len(),
        0,
        |_, _| Ok(pairs.to_vec()),
    )
}

/// Test wrapper for [`crate::terms::make_float_rooted`].
pub(crate) fn make_float(ctx: &mut dyn ExecutionContext, value: f64) -> Result<Eterm> {
    crate::terms::make_float_rooted(&mut scratch(), ctx, value)
}

/// Test wrapper for [`crate::terms::make_integer_rooted`].
pub(crate) fn make_integer(ctx: &mut dyn ExecutionContext, value: i64) -> Result<Eterm> {
    crate::terms::make_integer_rooted(&mut scratch(), ctx, value)
}

/// Test wrapper for [`crate::terms::make_heap_binary_rooted`].
pub(crate) fn make_heap_binary(ctx: &mut dyn ExecutionContext, bytes: &[u8]) -> Result<Eterm> {
    crate::terms::make_heap_binary_rooted(&mut scratch(), ctx, bytes)
}

/// Test wrapper for [`crate::terms::badmatch_reason_rooted`].
pub(crate) fn badmatch_reason(
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    value: Eterm,
) -> Result<Eterm> {
    crate::terms::badmatch_reason_rooted(
        &mut scratch(),
        ctx,
        RootSet::ALL_LIVE,
        module,
        |_, _| Ok(value),
    )
}

/// A test-only rooted reserve for callers that must fabricate a raw block (the
/// records test harness building a reference record block).
pub(crate) fn reserve_words(
    ctx: &mut dyn ExecutionContext,
    n: usize,
) -> Result<*mut Eterm> {
    let mut empty = scratch();
    empty.alloc_rooted(ctx, n, RootSet::ALL_LIVE)
}
