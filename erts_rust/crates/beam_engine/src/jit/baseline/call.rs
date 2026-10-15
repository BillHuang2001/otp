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
//! The baseline tier's **frame-preserving local call** lowering: the continuation store
//! for `Instr::Call`.
//!
//! The **tail** forms (`call_last` / `call_only`) need no continuation: they reuse the
//! caller's frame and are linked natively by `super::emit::link`. The
//! frame-**preserving** `call` instead writes a `Continuation::Address` into the live
//! frame — Rust-side `Machine` state a compiled block cannot address — so its store goes
//! through the helper here ([`jit_call_store`]) while the transfer to the callee stays
//! the block linker's ([`super::emit::link`]).
//!
//! [`jit_call_store`]: self::jit_call_store
