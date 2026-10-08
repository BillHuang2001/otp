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

//! Residual [`beam_loader::Instr::Generic`] opcodes the interpreter reports.
//!
//! The loader preserves every opcode it does not model as a typed IR variant as
//! [`beam_loader::Instr::Generic`] (`beam_loader`'s "fallback" section). Every
//! opcode the Milestone-1 interpreter can execute has its own typed IR variant
//! now (e.g. `is_any_native_record` is [`beam_loader::Instr::IsAnyNativeRecord`],
//! executed by the record family), so a residual opcode is **always** the
//! documented typed `unsupported` error carrying the opcode's own `ops.tab` name
//! ([`beam_loader::Opcode::name`]) — never a panic and never a silently wrong
//! result.

use beam_common::Result;
use beam_loader::Opcode;

use crate::error;
use crate::Step;

/// Report a residual [`beam_loader::Instr::Generic`] opcode as unsupported.
///
/// `Err(error::unsupported(op.name()))` is the documented typed error every
/// opcode with no typed IR variant keeps — never a panic and never a silently
/// wrong result.
pub(crate) fn exec(op: Opcode) -> Result<Step> {
    Err(error::unsupported(op.name()))
}
