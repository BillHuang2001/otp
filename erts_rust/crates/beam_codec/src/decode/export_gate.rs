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

//! The **active-export seam**: the host hook the `binary_to_term/2 [safe]` flag
//! consults before it accepts an `EXPORT_EXT` fun.
//!
//! C's `EXPORT_EXT` arm (`../../erts/emulator/beam/external.c:5006-5040`) requires
//! more than a well-formed image once the `ERTS_DIST_EXT_BTT_SAFE` flag is set:
//!
//! ```c
//! if (flags & ERTS_DIST_EXT_BTT_SAFE) {
//!     if (!erts_active_export_entry(mod, name, arity)) goto error;
//! }
//! ```
//!
//! `erts_active_export_entry(m, f, a)` (`../../erts/emulator/beam/export.h:176-183`)
//! is `erts_find_export_entry(m, f, a, erts_active_code_ix())` — a lookup in the
//! **running node's active export table**, i.e. "is this `M:F/A` exported by a module
//! that is loaded *right now*?". The arity is the `unsigned int` of that signature, so
//! it is the low 32 bits of the wire arity — the truncation
//! [`Decoder::decode_export`](super::Decoder) already performs. Neither the
//! distribution decode (`flags = 0`) nor `NEW_FUN_EXT` has such a rule.
//!
//! That table is **runtime** state (loaded modules plus their export sets): this crate
//! owns the **check site**, the host owns the **registry**. [`ExportGate`] is
//! therefore an **optional host seam** in the same shape as
//! [`OffHeapBinaries`](crate::OffHeapBinaries) and [`AtomCache`](crate::AtomCache) — a
//! caller that installs one with
//! [`Decoder::with_export_gate`](crate::Decoder::with_export_gate) gets the C's
//! refusal, and a caller that installs none keeps the previous behaviour unchanged
//! (under `[safe]` the two name atoms are still resolved by **lookup**, but the
//! export's activity is not checked). The seam is inert unless the `[safe]` flag is
//! also set, because the C only asks under the flag.
//!
//! ## Why a trait object and not a `fn` pointer
//!
//! The predicate has to read the host's own registry (in this runtime, `beam_rt`'s
//! code server and the export set of every loaded module), which is *borrowed* state
//! owned by the caller — not a global. A `fn` pointer cannot capture it, and a
//! process-wide registry would put runtime state back inside the codec. A
//! `&dyn ExportGate` lets the host lend a borrowed view of its own state for the
//! duration of one decode, exactly as `&mut dyn OffHeapBinaries` does for off-heap
//! binary payloads.

/// A host that knows which `M:F/A` exports are active on this node.
///
/// Installed on a [`Decoder`](crate::Decoder) with
/// [`Decoder::with_export_gate`](crate::Decoder::with_export_gate) (or reached through
/// [`decode_term_with_options_and_gate`](crate::decode_term_with_options_and_gate) /
/// [`decode_term_with_options_and_gate_used`](crate::decode_term_with_options_and_gate_used),
/// which also cover the `COMPRESSED` wrapper).
///
/// The decoder calls it **only** under the `binary_to_term/2 [safe]` flag
/// ([`Decoder::with_safe`](crate::Decoder::with_safe)) and **only** for an
/// `EXPORT_EXT` (`113`) term, immediately after the wire arity has been validated and
/// truncated to 32 bits and before the fun is materialized — so a refused image leaves
/// nothing on the heap. A `false` answer makes the decode `BeamError::BadArg`, exactly
/// as C's `goto error` does.
///
/// The runtime (`beam_rt`) supplies the concrete implementation over its loaded-module
/// registry; the codec never sees that state.
pub trait ExportGate {
    /// Whether `module:function/arity` is exported by a module loaded **right now**
    /// (C `erts_active_export_entry`).
    ///
    /// * `module` / `function` are the two wire atoms rendered as names, so a lookup
    ///   in the host's own atom-keyed registry needs no atom indices.
    /// * `arity` is the **low 32 bits** of the wire arity (`2^32 + 7` ⇒ `7`), the C's
    ///   `unsigned int` parameter. A wire arity of `2^32 + 7` is therefore active iff
    ///   `M:F/7` is.
    ///
    /// The answer is a plain `bool`: a lookup that cannot be answered (an unknown
    /// module, an unloaded one, a name that is not in the host's registry) is `false`,
    /// which the decoder turns into `BeamError::BadArg` — never a wrong `true`.
    fn is_export_active(&self, module: &str, function: &str, arity: u32) -> bool;
}
