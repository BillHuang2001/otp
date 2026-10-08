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

//! Contract **A.3** — the NIF C ABI, expanded: `enif_*` behaviours driven end to end
//! through a real C `.so` loaded by `erlang:load_nif/2`.
//!
//! Owned file of the A.3 corpus workstream; its cases are added there. See [`super`]'s
//! module docs for the corpus conventions.

use super::Case;

/// The expanded A.3 NIF C ABI cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
