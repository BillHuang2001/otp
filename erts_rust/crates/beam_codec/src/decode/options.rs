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

//! The `binary_to_term/2` **decode options** seam: the `[safe]` flag, plus the
//! additive entry points that let a host reach it through a free function (and thus
//! through the `COMPRESSED` wrapper, which a borrowed [`Decoder`] cannot own).
//!
//! `binary_to_term/2` parses its option list at `external.c:2170-2171`
//! (`ctx.flags |= ERTS_DIST_EXT_BTT_SAFE`, `external.h:134`). The flag is honoured at
//! **every atom-materialization site**:
//!
//! * `external.c:4530-4539` — `dec_term`'s four atom arms; and
//! * `external.c:3073-3082` — `dec_atom`, the C's separate resolver for identifier
//!   node atoms, `EXPORT_EXT`/`NEW_FUN_EXT` module + function atoms, and record
//!   module/name/key atoms;
//!
//! both of which this crate funnels through the single
//! [`Decoder::atom_term`](super::Decoder) helper, so one flag covers them all. Only
//! the `[safe]` half of the C's option list is modelled here; every other option
//! (`[used]` is a *return* value, `{minor_version,…}` is an encoder option) needs no
//! flag.
//!
//! ## The `[safe]` active-export half
//!
//! C's `EXPORT_EXT` arm additionally requires the export to be **active** under the
//! flag (`if (!erts_active_export_entry(mod, name, arity)) goto error;`,
//! `external.c:5032-5036`). That predicate reads the running node's active export
//! table, which this codec does not own, so it is an **optional host seam**
//! ([`ExportGate`], `src/decode/export_gate.rs`): a host installs its code-server view
//! on the decoder and a refused `M:F/A` becomes `BadArg`; with no gate installed the
//! check is skipped and decoding is unchanged. Because a borrowed [`Decoder`] cannot
//! outlive the `COMPRESSED` wrapper's inflated buffer, the two gate-aware free
//! functions — [`decode_term_with_options_and_gate`] and
//! [`decode_term_with_options_and_gate_used`] — install the gate on **both** the plain
//! and the inflated path. The gate is decoder state rather than a [`DecodeOptions`]
//! field because it borrows live host state (like
//! [`OffHeapBinaries`](crate::OffHeapBinaries) and [`AtomCache`](crate::AtomCache)),
//! while `DecodeOptions` is a plain `Copy` configuration value.
//!
//! ## Which route a host should use
//!
//! * `binary_to_term/1`-shaped callers (no option list): keep
//!   [`Decoder::new`](super::Decoder::new) +
//!   [`Decoder::decode`](super::Decoder::decode), or, for a wired option set,
//!   [`decode_term_with_options`].
//! * `binary_to_term/2` without an active-export registry:
//!   [`decode_term_with_options_used`] — it installs `[safe]` **and** reports the
//!   consumed source bytes exactly like [`crate::decode_term_used`].
//! * `binary_to_term/2` **with** one: the same plus an [`ExportGate`], i.e.
//!   [`decode_term_with_options_and_gate_used`]. This is the entry point a
//!   `beam_bifs::binconv::decode_term_bif` adopting the active-export rule should call.
//!
//! Every existing free function is untouched: with [`DecodeOptions::default`] (all
//! fields unset) the option-aware entry points are bit-for-bit the flag-free,
//! identity-free decode.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::{Atom, AtomTable, Eterm};

use crate::decode::export_gate::ExportGate;
use crate::decode::{decompress_wrapper, Decoder};
/// The `binary_to_term/2` decode options this codec understands.
///
/// All fields default to "not installed", so `DecodeOptions::default()` decodes
/// exactly as the pre-existing entry points do — the type is deliberately a plain
/// public struct with public fields rather than a builder, because it is a
/// *configuration value* whose default is the "no options" case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeOptions {
    /// The `[safe]` flag (`ERTS_DIST_EXT_BTT_SAFE`, `external.c:2170-2171`).
    ///
    /// With it set an atom **name** on the wire is resolved by lookup only and a miss
    /// is `BeamError::BadArg` (`external.c:4530-4539`, `:3073-3082`); without it the
    /// name is interned. Index-carrying forms (`ATOM_CACHE_REF`,
    /// `ATOM_INTERNAL_REF2`/`ATOM_INTERNAL_REF3`) name an existing atom and are
    /// unaffected.
    pub safe: bool,
    /// The local node identity — its `sysname` atom **as interned in `atoms`** and its
    /// `creation` — installed exactly as [`Decoder::with_local_node`] does.
    pub local_node: Option<(Atom, u32)>,
    /// The expected `LOCAL_EXT` (`121`) node hash, installed exactly as
    /// [`Decoder::with_local_node_hash`] does.
    pub local_hash: Option<u32>,
}

impl<'a> Decoder<'a> {
    /// Sets the `binary_to_term/2 [safe]` flag.
    ///
    /// Under the flag an atom **name** on the wire must already be interned in the
    /// caller's [`AtomTable`]; a miss is `BeamError::BadArg` and nothing is interned
    /// — so an unexpected or hostile image cannot grow the receiving node's atom table
    /// (C `erts_atom_get` instead of `erts_atom_put`, `external.c:4529-4539`).
    /// Index-carrying atom forms are unaffected: they name an atom that already exists.
    ///
    /// Additive: without a call to this method the flag is `false` and decoding is
    /// byte-for-byte unchanged.
    pub fn with_safe(self) -> Decoder<'a> {
        Decoder { safe: true, ..self }
    }

    /// Sets or clears the `[safe]` flag in place — the non-consuming twin of
    /// [`Decoder::with_safe`], for a decoder already held in a variable.
    pub fn set_safe(&mut self, safe: bool) {
        self.safe = safe;
    }
}

/// Installs `options` on `decoder` — the shared body of the option-aware entry points.
fn apply<'a>(mut decoder: Decoder<'a>, options: &DecodeOptions) -> Decoder<'a> {
    if let Some((node, creation)) = options.local_node {
        decoder = decoder.with_local_node(node, creation);
    }
    if let Some(hash) = options.local_hash {
        decoder = decoder.with_local_node_hash(hash);
    }
    if options.safe {
        decoder = decoder.with_safe();
    }
    decoder
}

/// Decodes a `term_to_binary` image with an explicit [`DecodeOptions`] — the
/// option-aware twin of [`crate::decode_term`].
///
/// Semantics are exactly [`crate::decode_term`]'s plus whatever `options` installs. A
/// top-level `COMPRESSED` (`80`) wrapper is unwrapped here for the same reason (the
/// inflated body is an owned buffer the borrowed [`Decoder`] cannot hold), and the
/// plain path keeps `decode_term`'s strict [`Decoder::finish`].
///
/// # Errors
///
/// Every error [`crate::decode_term`] reports, plus (with `options.safe`) a
/// `BeamError::BadArg` for a wire atom name that is not already interned in `atoms`,
/// and (with `options.local_hash`) a mismatched `LOCAL_EXT` hash.
pub fn decode_term_with_options(
    bytes: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    options: &DecodeOptions,
) -> Result<Eterm> {
    if let Some((body, _consumed)) = decompress_wrapper(bytes)? {
        let mut decoder = apply(Decoder::new_body(&body), options);
        return decoder.decode(heap, atoms);
    }
    let mut decoder = apply(Decoder::new(bytes)?, options);
    let term = decoder.decode(heap, atoms)?;
    decoder.finish()?;
    Ok(term)
}

/// Like [`decode_term_with_options`], but also reports how many **source** bytes the
/// image occupied — the option-aware twin of [`crate::decode_term_used`], and the
/// entry point `binary_to_term/2` needs (it accepts `[safe]` *and* `[used]`).
///
/// Like [`crate::decode_term_used`] this does **not** call [`Decoder::finish`] on
/// either branch: the `used` count exists precisely to tolerate trailing bytes. For a
/// `COMPRESSED` image the count is the wrapper header plus the zlib stream (see
/// [`crate::compress::decompress_bounded_used`]).
///
/// # Errors
///
/// Every error [`decode_term_with_options`] reports.
pub fn decode_term_with_options_used(
    bytes: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    options: &DecodeOptions,
) -> Result<(Eterm, usize)> {
    if let Some((body, consumed)) = decompress_wrapper(bytes)? {
        let mut decoder = apply(Decoder::new_body(&body), options);
        let term = decoder.decode(heap, atoms)?;
        return Ok((term, consumed));
    }
    let mut decoder = apply(Decoder::new(bytes)?, options);
    let term = decoder.decode(heap, atoms)?;
    Ok((term, decoder.position()))
}

/// Installs `gate` on `decoder` — the `[safe]` active-export half of the two
/// gate-aware entry points.
///
/// The gate is installed unconditionally; the decoder consults it only under the
/// `[safe]` flag and only for `EXPORT_EXT`, so installing it with `options.safe`
/// false is a no-op (see [`ExportGate`]).
fn with_gate<'a>(decoder: Decoder<'a>, gate: &'a dyn ExportGate) -> Decoder<'a> {
    decoder.with_export_gate(gate)
}

/// Decodes a `term_to_binary` image with an explicit [`DecodeOptions`] **and** an
/// [`ExportGate`] — the `[safe]` active-export twin of
/// [`decode_term_with_options`].
///
/// Semantics are exactly [`decode_term_with_options`]'s, with one addition: under
/// `options.safe` an `EXPORT_EXT` (`113`) fun whose `M:F/A` the gate does not report as
/// active is `BeamError::BadArg` — the C's
/// `if (flags & ERTS_DIST_EXT_BTT_SAFE) { if (!erts_active_export_entry(mod, name,
/// arity)) goto error; }` (`external.c:5032-5036`). The gate is asked with the
/// **32-bit-truncated** arity (the C's `unsigned int` parameter). With `options.safe`
/// false it is not consulted at all, and neither the distribution decode nor
/// `NEW_FUN_EXT` ever consults it. A top-level `COMPRESSED` (`80`) wrapper is
/// unwrapped here — the same reason as [`decode_term_with_options`] — and the gate is
/// installed on **both** the inflated and the plain path, so compression cannot route
/// around the check.
///
/// # Why the gate is decoder state and not a [`DecodeOptions`] field
///
/// The gate is a **borrowed host object**, not a configuration value: like
/// [`OffHeapBinaries`](crate::OffHeapBinaries) and [`AtomCache`](crate::AtomCache) it
/// names live runtime state (the caller's code-server view) for the duration of one
/// decode, with a lifetime tied to the decoder. `DecodeOptions` stays a plain
/// `Copy` configuration struct of scalars that callers build exhaustively, so adding a
/// reference-typed field there would both break every struct literal and pretend the
/// borrow is a value.
///
/// # Errors
///
/// Every error [`decode_term_with_options`] reports, plus `BeamError::BadArg` for a
/// `[safe]` `EXPORT_EXT` whose export the gate refuses.
pub fn decode_term_with_options_and_gate(
    bytes: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    options: &DecodeOptions,
    gate: &dyn ExportGate,
) -> Result<Eterm> {
    if let Some((body, _consumed)) = decompress_wrapper(bytes)? {
        let mut decoder = with_gate(apply(Decoder::new_body(&body), options), gate);
        return decoder.decode(heap, atoms);
    }
    let mut decoder = with_gate(apply(Decoder::new(bytes)?, options), gate);
    let term = decoder.decode(heap, atoms)?;
    decoder.finish()?;
    Ok(term)
}

/// Like [`decode_term_with_options_and_gate`], but also reports how many **source**
/// bytes the image occupied — the gate-aware twin of
/// [`decode_term_with_options_used`], i.e. `binary_to_term/2` with both `[safe]`
/// (gated) and `[used]`.
///
/// The `used` convention is [`decode_term_with_options_used`]'s: no
/// [`Decoder::finish`], so trailing bytes are tolerated and uncounted, and a
/// `COMPRESSED` image reports its header plus the zlib stream.
///
/// # Errors
///
/// Every error [`decode_term_with_options_and_gate`] reports.
pub fn decode_term_with_options_and_gate_used(
    bytes: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    options: &DecodeOptions,
    gate: &dyn ExportGate,
) -> Result<(Eterm, usize)> {
    if let Some((body, consumed)) = decompress_wrapper(bytes)? {
        let mut decoder = with_gate(apply(Decoder::new_body(&body), options), gate);
        let term = decoder.decode(heap, atoms)?;
        return Ok((term, consumed));
    }
    let mut decoder = with_gate(apply(Decoder::new(bytes)?, options), gate);
    let term = decoder.decode(heap, atoms)?;
    Ok((term, decoder.position()))
}