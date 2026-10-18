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

//! **Call/frame-control variants on real compiler output**: `Call`, `CallLast`,
//! `CallOnly`, `CallExt`, `CallExtOnly`, `CallExtLast`, `DeallocateReturn`,
//! `InitYregs`, `FuncInfo`, `Bif0`, `Move`, `Trim`.
//!
//! These are the highest-weight instruction variants in the whole OTP corpus
//! (`FuncInfo` 103 749 static occurrences, `CallExt` 75 795, `Call` 82 110,
//! `DeallocateReturn` 54 479, `CallExtOnly` 36 692, `InitYregs` 34 782,
//! `CallLast` 25 778, `CallExtLast` 20 659, `Bif0` 2 687), so every one of them
//! needs a real-`.beam` proof rather than a probe. Note that `FuncInfo` only
//! *executes* when control enters a function at the entry label — an export entry
//! and an ordinary `call` target the label **after** it — so the row for it must
//! drive a shape that really enters there (see the row's doc comment).
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.
