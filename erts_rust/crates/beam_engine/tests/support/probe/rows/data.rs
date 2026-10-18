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
//! The **tuple / list / record** data group.
//!
//! Owns the 9 data-structure variants:
//!
//! `GetTupleElement, SetTupleElement, GetList, GetHd, GetTl, UpdateRecord,
//! IsNativeRecord, PutRecord, IsRecordAccessible` — plus the already-executing
//! `GetRecordElements, GetRecordField, GetRecordElementsId, UpdateRecordId,
//! IsAnyNativeRecord`, whose **success** paths are reachable and should be probed
//! rather than left on the `fail` branch.
//!
//! Mechanic: seed the operand registers from a `setup` prologue built out of
//! `beam_engine::terms` (a real tuple/list/cons, a `Module::records` definition for
//! the native-record family) and declare the resulting register or the documented
//! `fail` branch.

use super::super::{Probe, ProbeAtoms};

/// Not converted yet: the legacy census roster still drives this group's variants
/// with degenerate operands (classed `ROUND-2` in the census detail).
pub fn rows(_atoms: &ProbeAtoms) -> Vec<Probe> {
    Vec::new()
}
