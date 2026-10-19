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

//! Contract **A.3** — NIF *stateful* family observables (scaffold).
//!
//! `nif_so` / `nif_res` cover the load surface, `nif_abi*` the frozen structs and every
//! `enif_*` shape. This group pins the **behaviour** of the stateful family: the
//! monitor/demonitor round trip, `enif_select` delivery for a real descriptor, and resource
//! identity across a collection.

use super::Case;

/// The A.3 stateful-family group.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
