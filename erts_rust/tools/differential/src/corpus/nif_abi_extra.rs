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

//! Contract **A.3** — the NIF C ABI, widened: a second block of live `enif_*` cases.
//!
//! A continuation of [`super::nif_abi`]: each case drives one `enif_*` behaviour end to end
//! through a real C `.so` loaded by `erlang:load_nif/2` and compares both runtimes. Case ids
//! use the `nif_extra/…` prefix. See the node `CONTEXT.md` ("How to add a corpus case").

use super::Case;

/// The A.3 widened cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
