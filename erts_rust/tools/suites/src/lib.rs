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
//! `suites` — measure **real third-party and OTP test suites** under both runtimes.
//!
//! This is the "widen suite coverage cheaply" companion to `tools/differential` (which pins
//! hand-written cases) and `tools/bench` (which measures performance): it takes *someone
//! else's* test suite, runs it once under the system ERTS (the oracle) and once under the
//! Rust runtime with the **identical** `-pa` set and the **identical** `-eval` expression,
//! and records per-module / per-case pass counts for both sides.
//!
//! Two targets:
//!
//! * [`khepri`] — a third-party project (Khepri) with 44 eunit modules; the checkout is
//!   read-only, staged into `$TMPDIR`, built with `rebar3 as test compile`, and each module is
//!   run as `eunit:test(Module, [verbose])`.
//! * [`stdlib`] — OTP's own `common_test` suites (`lists_SUITE`, …), driven per case by a
//!   committed runner so no `ct` node/test-dir setup is needed.
//!
//! It is a **library + binary** crate: the binary is what a human or CI drives, the library
//! exists so `cargo test` can assert the parsers and the verdict matrix.
//!
//! The crate is deliberately **dependency-free** and a **standalone workspace** (see
//! `Cargo.toml`): it is a pure driver/observer, and its only contract with the runtime is the
//! documented external CLI (compat contract A.6).
#![forbid(unsafe_code)]
pub mod cli;
pub mod error;
pub mod eunit;
pub mod json;
pub mod khepri;
pub mod proc;
pub mod report;
pub mod run;
pub mod runtime;
pub mod stdlib;
pub mod target;
