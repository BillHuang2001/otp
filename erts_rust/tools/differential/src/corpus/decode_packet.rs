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

//! Contract **A.7** — `erlang:decode_packet/3` across the packet types.
//!
//! Placeholder seam: this group is filled in by its author. It carries [`Case`] cases over
//! the integer and name packet types (raw/`0`, `1`, `2`, `4`, `line`, `asn1`, `sunrm`,
//! `cdr`, `fcgi`, `tpkt`, `http`, `httph`, `http_bin`, `httph_bin`, …).

use super::Case;

pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
