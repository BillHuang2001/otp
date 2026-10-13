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

//! Contract **A.7** — the `re` (regular expression) BIF family.
//!
//! Placeholder seam: this group is filled in by its author. It carries [`Case`] cases over
//! `re:run/2,3`, `re:compile/1,2`, `re:replace/3,4` and the run/compile options, promoted
//! from the live-oracle case source in `crates/beam_bifs/src/re/oracle.rs` so agreement is
//! gate-checked rather than only crate-tested.

use super::Case;

pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
