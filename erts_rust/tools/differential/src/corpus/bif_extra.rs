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
//! A.7 — the `erlang` BIF family beyond the groups that already own a slice of it: the
//! `process_info` item set, the local-fun identity surface, and the remaining `erlang:*` BIFs.
//!
//! Expression cases (and module-run cases where a spawned process is needed), so a case pins the
//! probe's single output line and the exit status.

use super::Case;

/// The group's cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
