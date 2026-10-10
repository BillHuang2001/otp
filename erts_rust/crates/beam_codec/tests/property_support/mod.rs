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

#![allow(dead_code)]

//! Shared support for the deterministic property tests.
//!
//! Both `tests/property_roundtrip.rs` (the oracle-independent byte-identity
//! property) and `tests/property_oracle.rs` (the live `erl` cross-checks) include
//! this module with `mod property_support;`. The split keeps each *file* under the
//! repository's ~1000-line guideline without weakening a single assertion or losing
//! coverage — the generator, the fixed seed and the corpus are unchanged.
//!
//! The pieces:
//!
//! * [`generator`] — the in-file splitmix64 PRNG, the bounded recursive term
//!   generator that reaches every tag this codec can encode, the per-term image set
//!   (the `{minor_version, 0|1|2}` matrix plus the default image) and the
//!   byte-identity `round_trip` helper;
//! * [`render`] — Erlang **source** rendering of the renderable subset, for the oracle;
//! * [`compare`] — structural `=:=` between two terms (`terms_equal`);
//! * [`oracle`] — resolving and driving the system `erl` (mirrors `tools/differential`).
//!
//! The module is `#![allow(dead_code)]`: each test binary includes it and uses only
//! the part it needs, so an item used by one binary and not the other is not a
//! warning. Nothing is duplicated between the two test files.

pub mod compare;
pub mod generator;
pub mod oracle;
pub mod render;

/// Fixed seed: the corpus is identical on every run and every machine.
pub const SEED: u64 = 0x5EED_F00D_1234_5678;
/// Terms generated for the (oracle-independent) round-trip property.
pub const PROPERTY_TERMS: usize = 5_000;
/// Renderable terms handed to the oracle (keeps the cross-check a few spawns).
pub const ORACLE_LIMIT: usize = 300;
/// Recursion ceiling of the generator.
pub const MAX_DEPTH: u32 = 5;
/// Decode/build heap (word count) — generous for the bounded generator.
pub const HEAP_WORDS: usize = 1 << 18;
/// The `creation` encoded for internal id immediates.
pub const CREATION: u32 = 0;
/// The local-node sysname the internal-id images carry.
pub const SYSNAME: &str = "nonode@nohost";
