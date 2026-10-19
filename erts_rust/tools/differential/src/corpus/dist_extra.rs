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

//! Contract **A.5** — the *widened* live two-node surface (scaffold).
//!
//! The `dist/` group pins the handshake, node visibility, one `erpc:call/4` term image, a
//! UTF-8 atom and the first two-VM self-pair. This group widens it with the rest of the
//! `DOP_*` message families and the multi-node traffic a real distribution link carries.

use super::Case;

/// The widened A.5 live two-node group.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
