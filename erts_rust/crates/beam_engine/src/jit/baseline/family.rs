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
//! The baseline tier's **instruction-family** helper: the in-block lowering for the
//! opcodes the interpreter's dispatch loop does not arm itself but hands to
//! [`crate::families::dispatch`] (its `other` arm).
//!
//! Left to the generic per-instruction deopt ([`super::jit_step_one`] →
//! [`Machine::step_instruction`](crate::Machine)) each such instruction paid a whole
//! interpreter step: the deopt re-matches the instruction variant in the interpreter's
//! driver, resolves it through the shared runtime funnel and only then reaches the same
//! family body. A compiled block can reach that family body directly, which is what this
//! helper does — see [`jit_family`].
//!
//! [`jit_family`]: self::jit_family
