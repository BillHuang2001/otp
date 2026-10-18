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

//! **Receive/message variants on real compiler output**: `Send`, `LoopRec`,
//! `RemoveMessage`, `Wait`, `Timeout`, `RecvMarkerReserve`, `RecvMarkerBind`,
//! `RecvMarkerUse`, `RecvMarkerClear`.
//!
//! The receive path needs a **mailbox**: the rows drive a fixture whose function
//! sends to itself and then receives, through the same host the engine's
//! `receive` family runs against (see `tests/receive.rs` for the seam). The
//! `RecvMarker*` variants come from OTP's alias-based receive, which the current
//! compiler does emit (the committed `recv.beam` fixture contains all four).
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.
