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

//! **Bit-syntax variants on real compiler output**: `BsMatch`, `BsSetPosition`,
//! `BsGetPosition`, `BsGetTail`, `BsGetBinary2`, `BsSkipBits2`, `BsGetUtf16`.
//!
//! The bit-syntax match compiler emits these from ordinary Erlang patterns
//! (`<<A:8,B/binary>>`, `<<C/utf16>>`, a sized/`bits` segment, a failing segment
//! that must fall through to the next clause), so every row here is written in
//! plain Erlang and compiled with the system `erlc`.
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.
