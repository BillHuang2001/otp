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
//! `tcp_inet`'s `flush` override — the C's `tcp_inet_flush` (`inet_drv.c:12708`-`:12731`).
//!
//! A close reaches the driver's `flush` before its `stop` (the C `flush_port`,
//! `erts/emulator/beam/io.c:3659`-`:3700`, called from `erts_deliver_port_exit`'s CLOSING arm,
//! `:3946`-`:3956`), and this driver's answer decides whether the port may still drain its queue or
//! has to drop it:
//!
//! ```text
//! discard_output = !(desc->inet.event_mask & FD_WRITE) || (tcp_add_flags & TCP_ADDF_LINGER_ZERO)
//! ```
//!
//! so the queue is cleared (`tcp_clear_output`, `:11718`) only when write readiness is unarmed — no
//! peer can ever take another byte — or `{linger, {true, 0}}` is in effect, whose zero-linger close
//! aborts the connection with an `RST`. Otherwise the bytes stay queued and the port parks in
//! `PortState::Closing` until they are written.
//!
//! Nothing is pinned here yet.
