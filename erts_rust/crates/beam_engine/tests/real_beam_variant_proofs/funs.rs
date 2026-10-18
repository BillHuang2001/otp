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

//! **Fun/apply variants on real compiler output**: `MakeFun3`, `CallFun`,
//! `CallFun2`, `Apply`, `ApplyLast`.
//!
//! The closure rows must run a **compiled** lambda body (the engine enters it
//! through the host's `apply_fun`/`resolve_local_fun` handling of the real
//! `funs.beam`-style fun table), not a test hook that reimplements the body.
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.
