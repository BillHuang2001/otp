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
//! The **exception / catch-frame** group.
//!
//! Owns the 9 exception variants:
//!
//! `Badmatch, CaseEnd, IfEnd, TryCase, TryEnd, TryCaseEnd, CatchEnd, Badrecord,
//! Raise`.
//!
//! Mechanic: a `try`-family row runs **inside** a live catch frame (the handler is
//! an aux block that moves the class/reason/`x(0)` it was handed into the success
//! sink), so a structured reason is observed by landing it in that frame rather
//! than by pinning an atom; a bare raise row declares `Expect::Raises { class,
//! reason }` with the tag atom the canonical probe universe provides.

use super::super::{Probe, ProbeAtoms};

/// Not converted yet: the legacy census roster still drives this group's variants
/// with degenerate operands (classed `ROUND-2` in the census detail).
pub fn rows(_atoms: &ProbeAtoms) -> Vec<Probe> {
    Vec::new()
}
