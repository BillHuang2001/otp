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

//! Oracle-cross-checked `INET_REQ_GETSTAT` cover for the `tcp_inet` **send** counters.
//!
//! A sibling `#[path]` test module of `stat.rs` so the source half stays small. Every case is
//! measured against the system `erl` (OTP 29.1.1 / erts-17.1) and skips **loudly** when it is
//! absent. `stat.rs` is the arithmetic; this file proves the drivers reach it at the right
//! counting site, with the right length, once per command.
