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

//! **Exception/control-flow variants on real compiler output**: `Badmatch`,
//! `CaseEnd`, `IfEnd`, `Raise`, `TryCase`, `TryCaseEnd`, `TryEnd`, `CatchEnd`.
//!
//! The census counts an instruction **before** its body runs, so a raising
//! variant (`Badmatch`, `CaseEnd`, `IfEnd`, `Raise`) is counted as executed *and*
//! its raise is the run's observable — compared against the oracle's exception
//! reason. A raise that lands in a live in-machine `try`/`catch` comes back `Ok`,
//! counted as executed but not as an error, which is how the `TryCase`/`CatchEnd`
//! rows read it.
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.
