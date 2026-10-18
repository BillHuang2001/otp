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
//! The **receive / message-queue** group.
//!
//! Owns the 10 receive variants:
//!
//! `Send, LoopRec, LoopRecEnd, Wait, Timeout, RemoveMessage, RecvMarkerReserve,
//! RecvMarkerBind, RecvMarkerClear, RecvMarkerUse`.
//!
//! Mechanic: install [`ProbeMailbox`](super::super::ProbeMailbox) (pre-seeded
//! through `push`, or via `resume_message` for the wakeup path) so the seam has
//! something to scan; a row that legitimately blocks declares
//! `Expect::Suspends(..)` and the harness resumes it at the reported
//! `(func, pc)` — never a typed error.

use super::super::{Probe, ProbeAtoms};

/// Not converted yet: the legacy census roster still drives this group's variants
/// with degenerate operands (classed `ROUND-2` in the census detail).
pub fn rows(_atoms: &ProbeAtoms) -> Vec<Probe> {
    Vec::new()
}
