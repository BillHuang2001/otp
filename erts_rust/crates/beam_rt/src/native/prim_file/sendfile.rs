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

//! `prim_file:sendfile/8` — the userspace sendfile of the runtime-native `prim_file:*` floor
//! (contract A.7).
//!
//! The oracle delegates `prim_inet:sendfile/4` to the driver's `TCP_REQ_SENDFILE` opcode, which
//! `beam_io` deliberately refuses; this module reads the requested extent from the descriptor's
//! own host file and writes it through the port command seam instead.
