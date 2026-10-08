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

//! Data-only chunks captured verbatim: `StrT`, `Attr`, `CInf`, `Meta`.
//!
//! None of these are interpreted *at this layer*: their payloads are opaque bytes here
//! (`StrT` becomes [`crate::ir::Module::strings`]; `Attr`/`CInf`/`Meta` become
//! [`crate::ir::Module::attributes`] / [`crate::ir::Module::compile_info`] /
//! [`crate::ir::Module::meta`]). The `Attr` bytes are additionally *decoded* into a
//! term one layer up ([`crate::attributes::AttributeTable`], on
//! [`crate::ir::Module::attributes_term`]).
//!
//! The debug chunks `DbgB`/`Dbgi` are recognized ids whose payloads are deliberately
//! neither parsed nor captured (tooling-only); see [`crate::chunk::classify`] for the
//! full per-id classification.
//!
//! (`Recs` — the native-record table — is *not* data-only: it is decoded by
//! [`crate::chunk::record`] into [`crate::ir::Module::records`].)

/// Copies a raw chunk payload verbatim.
///
/// Used for the data-only chunks (`StrT`, `Attr`, `CInf`, `Meta`); the copy is
/// deliberate because [`crate::ir::Module`] outlives the borrowed container.
pub fn raw(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}
