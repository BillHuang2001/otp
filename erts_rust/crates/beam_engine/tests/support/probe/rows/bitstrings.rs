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
//! The **bitstring / match-context** group.
//!
//! Owns the 16 bitstring variants:
//!
//! `BsMatch, BsMatchString, BsGetTail, BsGetPosition, BsSetPosition, BsGetInteger2,
//! BsGetFloat2, BsGetBinary2, BsSkipBits2, BsTestTail2, BsGetUtf8, BsGetUtf16,
//! BsGetUtf32, BsSkipUtf8, BsSkipUtf16, BsSkipUtf32`.
//!
//! Mechanic: the prologue opens a real match context (`Instr::BsStartMatch3`) over
//! a binary the row seeds, so the extraction rows address a live context; a
//! failed match/read declares `Expect::Branched` (the documented `fail` branch),
//! never a typed error.

use super::super::{Probe, ProbeAtoms};

/// Not converted yet: the legacy census roster still drives this group's variants
/// with degenerate operands (classed `ROUND-2` in the census detail).
pub fn rows(_atoms: &ProbeAtoms) -> Vec<Probe> {
    Vec::new()
}
