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

//! Contract **A.7** — the `erl_ddll` dynamic-driver-loader surface.
//!
//! Placeholder seam: this group is filled in by its author. It carries [`Case`] cases over
//! `erl_ddll:loaded_drivers/0`, `try_load/3`, `try_unload/2` and `info/2`, pinning only the
//! shapes/reasons on which the two runtimes agree.

use super::Case;

pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
