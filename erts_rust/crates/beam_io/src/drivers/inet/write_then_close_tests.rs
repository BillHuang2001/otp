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
//! The **read path's ordering rule**: a complete buffered unit **wins over** a pending close.
//!
//! `tcp_recv` (`inet_drv.c:13130`) classifies what the drain collected **before** it acts on the
//! end-of-file, and its delivery points (`:13216`-`:13217`, `:13229`-`:13230`) run first: a
//! `{packet, N}` frame that completed in the same call as the peer's end-of-file reaches the owner,
//! and only a **later** call's `n == 0` goes to `tcp_recv_closed` (`:13206`-`:13208`). The same
//! ordering holds on the enqueue side, where the read comes first and the arming second
//! (`:12309`, then `driver_select` at `:12315`), so a payload already in hand is never starved by
//! the close that arrived with it.
//!
//! Nothing is pinned here yet.
