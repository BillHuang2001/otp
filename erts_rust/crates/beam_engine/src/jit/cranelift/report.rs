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

//! The compile-time codegen report shared by every part of the tier.
//!
//! The codegen records what it actually inlined in a [`CodegenReport`] while it
//! lowers a module, and the artifact ([`super::CompiledFunctions`]) exposes the
//! result. Keeping the report in its own module is what lets `codegen`, `inline`
//! and `shape` all record observations into the same type.

use super::inline::SmallArith;
use beam_loader::TestKind;

/// Compile-time observations about one module's generated code.
///
/// The codegen records what it actually inlined here so the artifact
/// ([`CompiledFunctions`]) can expose it: the highest natively addressed x-register
/// (the driver's native-entry guard reads it), the set of inlined `test` kinds, the
/// set of inlined small-int `gc_bif` ops and the set of inlined comparison BIFs
/// (evidence that the fast paths — not `jit_step_one` — ran).
#[derive(Default)]
pub(super) struct CodegenReport {
    /// The highest x-register index any inlined instruction addresses natively.
    pub(super) highest_native_register: Option<u32>,
    /// The `test` kinds inlined natively (deduped, in first-inlined order).
    pub(super) inlined_tests: Vec<TestKind>,
    /// The small-int `gc_bif` ops inlined natively (deduped, in first-inlined
    /// order).
    pub(super) inlined_gc_bifs: Vec<SmallArith>,
    /// The comparison-BIF operators inlined natively (deduped, in first-inlined
    /// order), named by their `erlang` BIF spelling (`"=:="`, `"<"`, …).
    pub(super) inlined_comparisons: Vec<&'static str>,
    /// How many `select_val` instructions were inlined natively.
    pub(super) inlined_select_vals: usize,
    /// The tuple-read kinds inlined natively (deduped, in first-inlined order):
    /// `"get_tuple_element"` / `"select_tuple_arity"`.
    pub(super) inlined_tuple_reads: Vec<&'static str>,
    /// The list-read kinds inlined natively (deduped, in first-inlined order):
    /// `"get_list"` / `"get_hd"` / `"get_tl"`.
    pub(super) inlined_list_reads: Vec<&'static str>,
}

impl CodegenReport {
    /// Record that x-register `x` is touched natively at `[x_regs + x*8]`.
    pub(super) fn touch(&mut self, x: u32) {
        self.highest_native_register = Some(match self.highest_native_register {
            Some(previous) => previous.max(x),
            None => x,
        });
    }

    /// Record that a `test` of `kind` was inlined natively.
    pub(super) fn inlined_test(&mut self, kind: TestKind) {
        if !self.inlined_tests.contains(&kind) {
            self.inlined_tests.push(kind);
        }
    }

    /// Record that a `gc_bif` of `op` was inlined natively.
    pub(super) fn inlined_gc_bif(&mut self, op: SmallArith) {
        if !self.inlined_gc_bifs.contains(&op) {
            self.inlined_gc_bifs.push(op);
        }
    }

    /// Record that the comparison BIF `op` (its `erlang` spelling) was inlined
    /// natively.
    pub(super) fn inlined_comparison(&mut self, op: &'static str) {
        if !self.inlined_comparisons.contains(&op) {
            self.inlined_comparisons.push(op);
        }
    }

    /// Record that a `select_val` was inlined natively.
    pub(super) fn inlined_select_val(&mut self) {
        self.inlined_select_vals += 1;
    }

    /// Record that a tuple read of `kind` was inlined natively.
    pub(super) fn inlined_tuple_read(&mut self, kind: &'static str) {
        if !self.inlined_tuple_reads.contains(&kind) {
            self.inlined_tuple_reads.push(kind);
        }
    }

    /// Record that a list read of `kind` was inlined natively.
    pub(super) fn inlined_list_read(&mut self, kind: &'static str) {
        if !self.inlined_list_reads.contains(&kind) {
            self.inlined_list_reads.push(kind);
        }
    }
}
