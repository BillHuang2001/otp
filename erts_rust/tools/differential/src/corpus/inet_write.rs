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

//! Contract **A.4** — the inet **write** data path (scaffold).
//!
//! `inet_control` pins the framed **read** (`{packet,4}` / `{packet,2}` / `line`, in all
//! three block modes) plus the option/address store. This group pins the other direction:
//! what `gen_tcp:send/2` actually puts on the wire when a framing option is in force — the
//! length header, the segment split, and the passive-mode `{active, false}` write.

use super::Case;

/// The A.4 inet write-path group.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
