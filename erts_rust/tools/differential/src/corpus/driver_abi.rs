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

//! Contract **A.4** — the driver C ABI, expanded: the driver callback slots, `driver_async`,
//! `stop_select` and `ErlDrvTerm` output round trips, driven through a real C driver `.so`.
//!
//! Owned file of the A.4 corpus workstream; its cases are added there.

use super::Case;

/// The expanded A.4 driver C ABI cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
