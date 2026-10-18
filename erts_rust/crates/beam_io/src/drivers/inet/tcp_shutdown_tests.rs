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
//! The `TCP_REQ_SHUTDOWN`(44) guard ladder and its bad-operand reply.
//!
//! `tcp_inet_ctl`'s shutdown arm (`inet_drv.c:12343`-`:12370`) tests the **connection** first
//! (`!IS_CONNECTED` ⇒ `ctl_error(ENOTCONN)`) and only then the operand length
//! (`len != 1` ⇒ `ctl_error(EINVAL)`), so both faults are `[0]`-tagged **replies** — never a
//! `badarg` raise — and the order decides which one an unconnected port with a bad operand sees.
//! The rows here are cross-checked byte-for-byte against the system `erl`.
