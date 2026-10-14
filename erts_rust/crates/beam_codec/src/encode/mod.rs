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

//! The `Encoder` seam: terms → external term format bytes.
//!
//! One encoder serves both `term_to_binary` bytes (with the leading
//! [`VERSION_MAGIC`](crate::VERSION_MAGIC)) and distribution payload/ETS bodies
//! (without it) — the constructor decides which. Term inspection goes through
//! [`crate::term`]; this module owns only the *bytes*. The fun/export encoders
//! live in the child [`fun`] module, the native-record encoder in the child
//! [`record`] module and the `FLOAT_EXT` (minor_version 0) text formatter in the
//! child [`float`] module, each as a second `impl` block or helper on [`Encoder`].
//!
//! This module is safe Rust, like its children [`fun`], [`record`] and [`float`].

mod float;
mod fun;
mod map;
mod record;
use beam_common::Result;
use beam_term::prelude::{
    atom_val, is_atom, is_internal_pid, is_internal_port, is_list, is_nil, is_small, make_atom,
    pid_number, pid_serial, port_number, signed_value,
};
use beam_term::{Atom, AtomTable, Eterm};

use crate::atom_cache::AtomCacheWriter;
use crate::error;
use crate::tag::{
    ATOM_CACHE_REF_EXT, ATOM_EXT, ATOM_UTF8_EXT, BINARY_EXT, BIT_BINARY_EXT, COMPRESSED_EXT,
    FLOAT_EXT, INTEGER_EXT, LARGE_BIG_EXT, LARGE_TUPLE_EXT, LIST_EXT, LOCAL_EXT, MAP_EXT,
    NEWER_REFERENCE_EXT, NEW_FLOAT_EXT, NEW_PID_EXT, NIL_EXT, SMALL_ATOM_EXT,
    SMALL_ATOM_UTF8_EXT, SMALL_BIG_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, STRING_EXT, V4_PORT_EXT,
    Tag,
};
use crate::term::{
    big_parts, bit_count_to_bytes, bitstring_bytes, cons_head, cons_tail, external_pid_parts,
    external_port_parts, external_ref_parts, flatmap_parts, float_value, fun_kind_term, is_record,
    map_pairs_storage_order, tuple_arity, tuple_element, FunPid,
};
use crate::writer::Writer;

/// `MAX_ARITYVAL` (`erl_term.h:322`): the largest flat-map pair count, bound by the
/// `arityval` header's 24-bit size field.
const MAX_ARITYVAL: usize = (1 << 24) - 1;

/// `SMALL_BIG_EXT` carries an 8-bit **byte** count (the number of base-256 digits on
/// the wire, `external.c:3587`); larger magnitudes use `LARGE_BIG_EXT`.
const SMALL_BIG_BYTE_MAX: usize = 255;

/// The OTP atom-name length limit, in **characters** (not bytes).
const ATOM_CHAR_MAX: usize = 255;

/// Which atom tag the encoder emits.
///
/// Atoms are interned, never compared by name: the encoder renders each name
/// from the caller's [`AtomTable`] in the selected encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AtomEncoding {
    /// Emit `SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT` — exactly what `term_to_binary`
    /// emits. This is the default.
    Utf8,
    /// Emit `SMALL_ATOM_EXT`/`ATOM_EXT` (Latin-1) for nodes that did not
    /// negotiate `DFLAG_UTF8_ATOMS` (used by `beam_dist`).
    Latin1,
}

/// The `term_to_binary/2` `{minor_version, N}` wire flavour.
///
/// `parse_t2b_opts` (`external.c:1501-1516`) maps the option onto a set of encoder
/// dflags; this enum names the three resulting flavours. It is **independent of**
/// [`AtomEncoding`] and only meaningful for `term_to_binary`-shaped output
/// ([`Encoder::new`]); the [`Encoder::new_body`] distribution/ETS form keeps its
/// own [`AtomEncoding`] seam.
///
/// The **default** is [`MinorVersion::V2`], so an [`Encoder`] built with
/// [`Encoder::new`]/[`encode_term`] produces exactly the bytes it always did
/// (UTF-8 atoms + `NEW_FLOAT_EXT`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum MinorVersion {
    /// `{minor_version, 0}` — the pre-OTP-17 / BER-era flavour: `DFLAG_UTF8_ATOMS`
    /// **and** `DFLAG_NEW_FLOATS` clear, so Latin-1 atoms are `ATOM_EXT` (`100`)
    /// and floats are the 31-byte text `FLOAT_EXT` (`99`).
    V0,
    /// `{minor_version, 1}` — the OTP-17-era default: `DFLAG_UTF8_ATOMS` clear but
    /// `DFLAG_NEW_FLOATS` set, so atoms are `ATOM_EXT` (`100`) and floats are the
    /// modern `NEW_FLOAT_EXT` (`70`).
    V1,
    /// `{minor_version, 2}` — the modern default, identical to
    /// `TERM_TO_BINARY_DFLAGS` (`dist.h:138-139`): UTF-8 atoms +
    /// `NEW_FLOAT_EXT`. This is [`Encoder::new`]'s behaviour.
    #[default]
    V2,
}

/// The resolved atom-tag family for a given `(minor_version, atom_encoding)` pair —
/// the crate-internal projection of the C `enc_atom` decision
/// (`external.c:2906-2950`). Distinct from the public [`AtomEncoding`] because the
/// `term_to_binary` Latin-1 form (all `ATOM_EXT`, UTF-8 fallback for a non-Latin-1
/// name) is *not* the distribution [`AtomEncoding::Latin1`] form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AtomFlavour {
    /// `SMALL_ATOM_UTF8_EXT`(`119`)/`ATOM_UTF8_EXT`(`118`) — `DFLAG_UTF8_ATOMS` set.
    Utf8,
    /// `SMALL_ATOM_EXT`(`115`)/`ATOM_EXT`(`100`) for a Latin-1 name, `unsupported`
    /// for a non-Latin-1 one — `DFLAG_UTF8_ATOMS` clear, `DFLAG_SMALL_ATOM_TAGS` set
    /// (the distribution [`AtomEncoding::Latin1`] flavour, unchanged).
    Latin1SmallTags,
    /// `ATOM_EXT`(`100`) for **every** Latin-1 name regardless of length, falling
    /// back to the UTF-8 tags for a non-Latin-1 name — `DFLAG_UTF8_ATOMS` and
    /// `DFLAG_SMALL_ATOM_TAGS` both clear (the `{minor_version,0|1}` flavour;
    /// `external.c:2924`/`:2937`).
    Latin1WideOnly,
}

/// The identity of a [`FunKind::Local`](crate::term::FunKind) fun that its **term
/// layout does not carry** but the `NEW_FUN_EXT` (`112`) wire form requires.
///
/// A `make_fun3` closure (`beam_engine`/`beam_rt`) inlines only its engine-internal
/// descriptor (`[uniq: u64][index: u32]`); it stores **no** module atom, no 16-byte
/// module MD5, no `old_index`/`old_uniq` and no creator pid. The C encoder
/// (`external.c:3966-3993`) resolves those fields through the fun's `ErlFunEntry`
/// (the running module's lambda table), which only the runtime knows — so they are
/// an **optional caller input**, supplied through [`Encoder::with_fun_identity`].
/// The codec never invents them: without one a local fun stays
/// [`crate::error::unsupported_local_fun`] (classify-don't-lie).
///
/// The field meanings mirror `ErlFunEntry` (`erts/emulator/beam/erl_fun.h:31-59`):
/// `module`/`uniq` identify the compiled module, `old_index`/`old_uniq` are its
/// legacy lambda identity, and `creator` is the pid the C encoder writes — for a
/// local fun that is always `erts_init_process_id` (the `init` process), **not** the
/// fun's own creator (`external.c:3989`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FunIdentity {
    /// The module the closure was compiled in (`ErlFunEntry.module`).
    pub module: Atom,
    /// The module's 16-byte MD5 (`ErlFunEntry.uniq`).
    pub uniq: [u8; 16],
    /// The legacy lambda index (`ErlFunEntry.old_index`).
    pub old_index: u32,
    /// The legacy lambda unique number (`ErlFunEntry.old_uniq`).
    pub old_uniq: u32,
    /// The creator pid written to the wire — `erts_init_process_id` for a local fun.
    pub creator: FunPid,
}

impl FunIdentity {
    /// Builds a fun identity from its five wire fields.
    pub fn new(
        module: Atom,
        uniq: [u8; 16],
        old_index: u32,
        old_uniq: u32,
        creator: FunPid,
    ) -> FunIdentity {
        FunIdentity {
            module,
            uniq,
            old_index,
            old_uniq,
            creator,
        }
    }
}

/// Incremental external term format encoder.
///
/// Holds a borrowed atom table (the sole source of atom names) and an output
/// buffer; `new` emits the leading version byte, `new_body` does not.
pub struct Encoder<'a> {
    /// The atom table atom names are rendered from.
    atoms: &'a AtomTable,
    /// The output buffer.
    buf: Writer,
    /// The selected atom tag family.
    atom_encoding: AtomEncoding,
    /// The `term_to_binary/2` wire flavour. [`MinorVersion::V2`] (the default)
    /// defers to `atom_encoding` and emits `NEW_FLOAT_EXT`; the older flavours
    /// override both (see [`MinorVersion`] and [`Encoder::set_minor_version`]).
    minor_version: MinorVersion,
    /// The outbound atom cache, when the caller installed one via
    /// [`Encoder::with_atom_cache`]. `None` writes every atom inline.
    atom_cache: Option<&'a mut dyn AtomCacheWriter>,
    /// The local node identity (`sysname` atom + `creation`) used to encode
    /// internal pid/port immediates as `NEW_PID_EXT`/`V4_PORT_EXT`. Node identity
    /// is owned by the runtime, so it is an optional input; without it an internal
    /// id has no wire image.
    local_node: Option<(Atom, u32)>,
    /// The fun identity supplied by the caller via [`Encoder::with_fun_identity`],
    /// used to encode a [`FunKind::Local`](crate::term::FunKind) fun as
    /// `NEW_FUN_EXT`. `None` — the default — keeps such a fun
    /// [`crate::error::unsupported_local_fun`] (the identity cannot be invented).
    fun_identity: Option<FunIdentity>,
    /// The `term_to_binary/2` `deterministic` option (`DFLAG_DETERMINISTIC`): when
    /// set, a map's pairs are emitted sorted by **full term order** on the key
    /// rather than in the map's stored order (see [`Encoder::set_deterministic`]).
    /// `false` — the default — reproduces today's bytes exactly.
    deterministic: bool,
}
impl<'a> Encoder<'a> {
    /// Creates an encoder whose output begins with the version byte `131`.
    pub fn new(atoms: &'a AtomTable) -> Encoder<'a> {
        let mut encoder = Encoder::new_body(atoms);
        encoder.buf.u8(crate::VERSION_MAGIC);
        encoder
    }

    /// Creates an encoder for a body: **no** leading version byte.
    ///
    /// Used for the distribution payload that follows a distribution header and
    /// for ETS term blobs.
    pub fn new_body(atoms: &'a AtomTable) -> Encoder<'a> {
        Encoder {
            atoms,
            buf: Writer::new(),
            atom_encoding: AtomEncoding::Utf8,
            minor_version: MinorVersion::V2,
            atom_cache: None,
            local_node: None,
            fun_identity: None,
            deterministic: false,
        }
    }
    /// Selects the atom tag family; defaults to [`AtomEncoding::Utf8`].
    ///
    /// Ignored when a [`MinorVersion`] other than [`MinorVersion::V2`] is selected:
    /// the `{minor_version,0|1}` flavours pin the atom tags themselves (see
    /// [`Encoder::set_minor_version`]).
    pub fn set_atom_encoding(&mut self, encoding: AtomEncoding) {
        self.atom_encoding = encoding;
    }

    /// Selects the `term_to_binary/2` [`MinorVersion`] wire flavour.
    ///
    /// The default is [`MinorVersion::V2`], which reproduces today's bytes exactly
    /// (`atom_encoding` respected, `NEW_FLOAT_EXT` floats). The older flavours
    /// override the atom flavour and, for [`MinorVersion::V0`], the float form:
    ///
    /// | flavour | atoms | floats |
    /// |---|---|---|
    /// | [`MinorVersion::V0`] | `ATOM_EXT` (`100`) for Latin-1 names, UTF-8 tags otherwise | `FLOAT_EXT` (`99`, 31-byte text) |
    /// | [`MinorVersion::V1`] | `ATOM_EXT` (`100`) for Latin-1 names, UTF-8 tags otherwise | `NEW_FLOAT_EXT` (`70`) |
    /// | [`MinorVersion::V2`] | [`AtomEncoding`] (UTF-8 by default) | `NEW_FLOAT_EXT` (`70`) |
    pub fn set_minor_version(&mut self, minor_version: MinorVersion) {
        self.minor_version = minor_version;
    }

    /// Builder form of [`Encoder::set_minor_version`].
    pub fn with_minor_version(mut self, minor_version: MinorVersion) -> Encoder<'a> {
        self.minor_version = minor_version;
        self
    }

    /// Selects the `term_to_binary/2` `deterministic` option
    /// (`DFLAG_DETERMINISTIC`, `parse_t2b_opts` `external.c:1497-1499`/`:1551-1553`).
    ///
    /// When set, a map's `(key, value)` pairs are collected and **sorted by the key
    /// in full Erlang term order** before being emitted, for **both** a ≤ 32-pair
    /// flat map and a > 32-pair hash map (HAMT) — exactly the C behaviour
    /// (`map_key_compare` = `CMP_TERM` = `erts_cmp(a, b, 1, 0)`, `external.c:3217-3227`,
    /// applied at `external.c:3757-3764` and `:3783-3807`). This makes the image
    /// independent of the map's internal storage order.
    ///
    /// The default is `false`, so every existing signature and every un-flavoured
    /// [`encode_term`] is byte-for-byte unchanged; the flag changes **only** map
    /// emission (atom encoding, floats and all other tags are untouched).
    pub fn set_deterministic(&mut self, on: bool) {
        self.deterministic = on;
    }

    /// Builder form of [`Encoder::set_deterministic`].
    pub fn with_deterministic(mut self) -> Encoder<'a> {
        self.deterministic = true;
        self
    }
    /// The atom-tag family in effect: [`AtomEncoding`] under [`MinorVersion::V2`],
    /// the pinned `term_to_binary` Latin-1 form under `V0`/`V1`.
    fn atom_flavour(&self) -> AtomFlavour {
        match self.minor_version {
            MinorVersion::V0 | MinorVersion::V1 => AtomFlavour::Latin1WideOnly,
            MinorVersion::V2 => match self.atom_encoding {
                AtomEncoding::Utf8 => AtomFlavour::Utf8,
                AtomEncoding::Latin1 => AtomFlavour::Latin1SmallTags,
            },
        }
    }

    /// Whether floats are written as `NEW_FLOAT_EXT` (`70`); clear only under
    /// [`MinorVersion::V0`] (`DFLAG_NEW_FLOATS`).
    fn uses_new_floats(&self) -> bool {
        !matches!(self.minor_version, MinorVersion::V0)
    }

    /// Installs an outbound atom cache ([`AtomCacheWriter`]).
    ///
    /// From here on [`Encoder::encode`] consults the cache for **every** atom and,
    /// when [`AtomCacheWriter::intern`] returns an index, writes
    /// `[ATOM_CACHE_REF_EXT (82), index]` instead of the inline
    /// `SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT` form (the selected [`AtomEncoding`] is
    /// likewise bypassed on the cached path). A [`None`] return falls through to the
    /// existing inline encoding unchanged, so an atom the cache does not take is
    /// byte-identical to an encoder without a cache.
    ///
    /// This is the send-side seam `beam_dist` drives (see [`AtomCacheWriter`] and
    /// [`crate::atom_cache`]); the legacy `NEW_CACHE` (`78`) tag is never emitted.
    pub fn with_atom_cache(mut self, cache: &'a mut dyn AtomCacheWriter) -> Encoder<'a> {
        self.atom_cache = Some(cache);
        self
    }

    /// The local node identity (`sysname` atom + `creation`) used to encode
    /// internal pid/port immediates as `NEW_PID_EXT`/`V4_PORT_EXT`. Without it an
    /// internal id has no wire image (the sysname/creation are node identity,
    /// owned by the runtime), so it reports `unsupported`.
    pub fn with_local_node(mut self, node: Atom, creation: u32) -> Self {
        self.local_node = Some((node, creation));
        self
    }

    /// Supplies the module identity needed to encode a
    /// [`FunKind::Local`](crate::term::FunKind) fun (`make_fun3` closure) as
    /// `NEW_FUN_EXT` (`112`).
    ///
    /// A local fun's term layout stores only its engine-internal descriptor and so
    /// carries none of the wire fields `NEW_FUN_EXT` needs; [`FunIdentity`] is the
    /// runtime's module identity (`beam_engine`/`beam_rt`). With it installed, the
    /// encoder emits a **faithful** `NEW_FUN_EXT` built from the *term's*
    /// arity/index/`num_free`/environment plus the *supplied* module/`uniq`/
    /// `old_index`/`old_uniq`/creator — byte-identical to the C encoder's local-fun
    /// path (`external.c:3966-3993`). Without it a local fun stays
    /// [`crate::error::unsupported_local_fun`] (never a wrong image).
    ///
    /// The default is `None`, so every existing signature and every un-flavoured
    /// [`encode_term`] is byte-for-byte unchanged.
    pub fn with_fun_identity(mut self, identity: FunIdentity) -> Self {
        self.fun_identity = Some(identity);
        self
    }

    /// Encodes `term`, appending it to the output.
    ///
    /// A tag the M1 term layer does not support yields
    /// [`crate::error::unsupported`]; a local (`make_fun3`) fun **without** a
    /// [`FunIdentity`] installed via [`Encoder::with_fun_identity`] yields
    /// [`crate::error::unsupported_local_fun`] (its wire fields are not derivable
    /// from the term); a value beyond a wire bound yields `BeamError::SystemLimit`.
    /// The encoder never silently mis-encodes.
    pub fn encode(&mut self, term: Eterm) -> Result<()> {
        // Classification order is deliberate (see the crate `CONTEXT.md`,
        // "`src/encode.rs`"): immediates first, then each boxed family through its
        // `Result`-returning accessor, which rejects the wrong type with `BadArg`.
        if is_small(term) {
            return self.encode_small(signed_value(term));
        }
        if is_atom(term) {
            return self.encode_atom(term);
        }
        if is_nil(term) {
            self.buf.u8(NIL_EXT);
            return Ok(());
        }
        if is_list(term) {
            return self.encode_list(term);
        }
        // Funs come before the boxed-family probes: a fun header carries a distinct
        // subtag, so the classification cannot overlap any other family.
        if let Ok(kind) = fun_kind_term(term) {
            return self.encode_fun(term, kind);
        }
        // Native records likewise: a record header carries its own subtag
        // (`RECORD_SUBTAG`), so `is_record` cannot collide with the tuple probe
        // below (`Tag::Record`, `external.c:4006-4036`).
        if is_record(term) {
            return self.encode_record(term);
        }
        if let Ok((negative, digits)) = big_parts(term) {
            return self.encode_bignum(negative, digits);
        }
        if let Ok(value) = float_value(term) {
            return self.encode_float(value);
        }
        if let Ok(arity) = tuple_arity(term) {
            return self.encode_tuple(term, arity);
        }
        if let Ok((keys, values)) = flatmap_parts(term) {
            return self.encode_map(keys, values);
        }
        // A > 32-key map is a *hash map* (HAMT), which `flatmap_parts` rejects. Its pairs
        // come back in the map's **stored** (ascending HAMT node-slot pre-order) order; the
        // C encoder walks that same tree through a LIFO work stack, i.e. in the *reverse*
        // order, so `encode_hashmap_pairs` reverses before emitting. This probe matches only
        // a genuine map, so it cannot shadow any term the flat path already handled (see
        // §"Map key encode order").
        if let Ok(pairs) = map_pairs_storage_order(term) {
            return self.encode_hashmap_pairs(&pairs);
        }
        if let Ok((node, number, serial, creation)) = external_pid_parts(term) {
            return self.encode_pid(node, number, serial, creation);
        }
        if let Ok((node, number, creation)) = external_port_parts(term) {
            return self.encode_port(node, number, creation);
        }
        if let Ok((node, ids, creation)) = external_ref_parts(term) {
            return self.encode_ref(node, ids, creation);
        }
        if let Ok((bytes, offset_bits, size_bits)) = bitstring_bytes(term) {
            return self.encode_bitstring(bytes, offset_bits, size_bits);
        }
        // Internal pids/ports are 60-bit `Eterm` immediates (not boxed external id
        // things) and an internal reference is the M1 `[header, node, serial, id]`
        // block — none of them carry a node identity of their own, so the local
        // node's sysname/creation — owned by the runtime — must be supplied via
        // [`Encoder::with_local_node`].
        if let Some((local_node, creation)) = self.local_node {
            // Reading the id-tag bits of any `Eterm` is a safe, frozen-layout
            // (`beam_term`) operation — no `unsafe` needed (the crate denies it).
            if is_internal_pid(term) {
                return self.encode_internal_pid(
                    local_node,
                    pid_number(term),
                    pid_serial(term),
                    creation,
                );
            }
            if is_internal_port(term) {
                return self.encode_internal_port(local_node, port_number(term), creation);
            }
            // An internal reference: `crate::term::internal_ref_id` is the safe
            // frozen-layout probe (`beam_term`'s `is_internal_ref` is `unsafe`, and the
            // crate confines `unsafe` to `crate::term`). It accepts exactly the M1
            // 4-word block and returns a typed `BadArg` for every other term — a
            // C-layout `ErtsORefThing` block included, whose id word does not exist —
            // so it doubles as the predicate rather than risking a wrong id.
            if let Ok(id) = crate::term::internal_ref_id(term) {
                return self.encode_internal_ref(local_node, id, creation);
            }
        }
        // Anything else is not a term the encoder can represent: report it explicitly
        // rather than emit a wrong encoding. A bitstring in *either* representation —
        // the on-heap `ErlHeapBits` block and the off-heap refc `BinRef` pair — is
        // handled above through `bitstring_bytes`, so this path is never a bitstring.
        Err(error::unsupported(Tag::Binary))
    }

    /// Encodes a small integer following the C width policy
    /// (`external.c:3542-3599`): `< 256` ⇒ `SMALL_INTEGER_EXT`, a value that fits a
    /// signed 32-bit ⇒ `INTEGER_EXT`, otherwise a bignum.
    fn encode_small(&mut self, value: i64) -> Result<()> {
        if (0..=255).contains(&value) {
            self.buf.u8(SMALL_INTEGER_EXT);
            self.buf.u8(value as u8);
            return Ok(());
        }
        if let Ok(int) = i32::try_from(value) {
            self.buf.u8(INTEGER_EXT);
            // `(int as u32).to_be_bytes()` is exactly `int.to_be_bytes()` (two's
            // complement big-endian), the `INTEGER_EXT` payload.
            self.buf.u32be(int as u32);
            return Ok(());
        }
        // The magnitude becomes a one-`ErtsDigit` bignum block; the sign is carried
        // separately.
        self.encode_bignum(value < 0, &[value.unsigned_abs()])
    }

    /// Encodes a bignum from its `(negative, digits)` parts: `digits` are
    /// `ErtsDigit`s (`u64`), least significant first. The wire carries the magnitude
    /// as base-256 bytes, least significant first, with the count of **significant
    /// bytes** in the tag's length field — exactly C `big_bytes`/`big_to_bytes`
    /// (`big.c:3405-3490`). High-order zero bytes are stripped; an all-zero magnitude
    /// is the small integer `0` on the wire.
    fn encode_bignum(&mut self, negative: bool, digits: &[u64]) -> Result<()> {
        let magnitude = digits_to_le_bytes(digits);
        let magnitude = strip_high_zero_bytes(&magnitude);
        if magnitude.is_empty() {
            // A zero magnitude coincides with the `SMALL_INTEGER_EXT` encoding of 0,
            // so decoding the output round-trips to a small integer, as in C.
            self.buf.u8(SMALL_INTEGER_EXT);
            self.buf.u8(0);
            return Ok(());
        }
        let sign = u8::from(negative);
        if magnitude.len() <= SMALL_BIG_BYTE_MAX {
            self.buf.u8(SMALL_BIG_EXT);
            self.buf.u8(magnitude.len() as u8);
        } else {
            // The byte count is bounded by `BIG_ARITY_MAX * sizeof(ErtsDigit)`
            // (65535 * 8 = 524280 on 64-bit), so it always fits the 32-bit
            // `LARGE_BIG_EXT` length field.
            self.buf.u8(LARGE_BIG_EXT);
            self.buf.u32be(magnitude.len() as u32);
        }
        self.buf.u8(sign);
        self.buf.bytes(magnitude);
        Ok(())
    }

    /// Encodes a float in the flavour's form: `NEW_FLOAT_EXT` (`70`) + 8 IEEE-754
    /// bytes (the modern form, `DFLAG_NEW_FLOATS`), or — under
    /// [`MinorVersion::V0`] — the legacy text `FLOAT_EXT` (`99`) with its fixed
    /// 31-byte payload (`external.c:3825-3849`; see [`crate::encode`]'s child
    /// [`float`] module).
    fn encode_float(&mut self, value: f64) -> Result<()> {
        if self.uses_new_floats() {
            self.buf.u8(NEW_FLOAT_EXT);
            self.buf.u64be(value.to_bits());
        } else {
            self.buf.u8(FLOAT_EXT);
            self.buf.bytes(&float::format_float_ext(value)?);
        }
        Ok(())
    }

    /// Encodes an atom in the selected [`AtomEncoding`] (or the pinned
    /// `{minor_version,0|1}` Latin-1 form, see [`Encoder::set_minor_version`]), or
    /// as `ATOM_CACHE_REF` (`82`) when an outbound atom cache
    /// ([`Encoder::with_atom_cache`]) knows it.
    fn encode_atom(&mut self, term: Eterm) -> Result<()> {
        // The cached path comes first (C `enc_atom`, `external.c:2877`): an atom the
        // outbound cache knows is written as `[82, index]` and needs no name at all —
        // the name travels in the distribution header. The `atoms` copy is taken
        // before borrowing `self.atom_cache` so the two disjoint fields do not
        // conflict.
        let atoms = self.atoms;
        let cached = match self.atom_cache.as_mut() {
            Some(cache) => cache.intern(atom_val(term), atoms),
            None => None,
        };
        if let Some(index) = cached {
            self.buf.u8(ATOM_CACHE_REF_EXT);
            self.buf.u8(index);
            return Ok(());
        }

        let name = self.atoms.name(atom_val(term)).ok_or_else(error::bad_arg)?;
        // OTP caps atom names at 255 *characters*, independent of the encoding.
        if name.chars().count() > ATOM_CHAR_MAX {
            return Err(error::system_limit());
        }
        match self.atom_flavour() {
            AtomFlavour::Utf8 => self.write_atom_utf8(name),
            AtomFlavour::Latin1SmallTags => self.write_atom_latin1_small_tags(name)?,
            AtomFlavour::Latin1WideOnly => self.write_atom_latin1_wide_only(name),
        }
        Ok(())
    }

    /// Writes `name` as `SMALL_ATOM_UTF8_EXT`(`119`)/`ATOM_UTF8_EXT`(`118`) — the
    /// `DFLAG_UTF8_ATOMS` branch of `enc_atom` (`external.c:2910-2922`).
    fn write_atom_utf8(&mut self, name: &str) {
        let bytes = name.as_bytes();
        if bytes.len() <= 255 {
            self.buf.u8(SMALL_ATOM_UTF8_EXT);
            self.buf.u8(bytes.len() as u8);
        } else {
            // At most 255 characters × 4 UTF-8 bytes ≤ 1020, so the 16-bit
            // length always fits.
            self.buf.u8(ATOM_UTF8_EXT);
            self.buf.u16be(bytes.len() as u16);
        }
        self.buf.bytes(bytes);
    }

    /// Writes `name` as `SMALL_ATOM_EXT`(`115`)/`ATOM_EXT`(`100`) Latin-1 bytes — the
    /// `DFLAG_SMALL_ATOM_TAGS` distribution branch of `enc_atom`
    /// (`external.c:2923-2935`), i.e. the public [`AtomEncoding::Latin1`] flavour.
    ///
    /// A name outside `U+0000..=U+00FF` has no Latin-1 image and is reported as
    /// [`error::unsupported`] — this flavour is deliberately narrower than the C
    /// (which would fall back to the UTF-8 tags); its consumers (`beam_dist`) rely
    /// on that, so it is **unchanged**.
    fn write_atom_latin1_small_tags(&mut self, name: &str) -> Result<()> {
        let bytes = latin1_bytes(name).ok_or_else(|| error::unsupported(Tag::SmallAtom))?;
        if bytes.len() <= 255 {
            self.buf.u8(SMALL_ATOM_EXT);
            self.buf.u8(bytes.len() as u8);
        } else {
            self.buf.u8(ATOM_EXT);
            self.buf.u16be(bytes.len() as u16);
        }
        self.buf.bytes(&bytes);
        Ok(())
    }

    /// Writes `name` in the `{minor_version,0|1}` Latin-1 form: `ATOM_EXT` (`100`)
    /// for **every** Latin-1-representable name regardless of length (because
    /// `DFLAG_SMALL_ATOM_TAGS` is clear, `external.c:2924`/`:2937`), falling back to
    /// the UTF-8 tags (`SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT`) when the name is not
    /// Latin-1 (`external.c:2910`: `utf8_atoms || a->latin1_chars < 0`).
    fn write_atom_latin1_wide_only(&mut self, name: &str) {
        match latin1_bytes(name) {
            Some(bytes) => {
                self.buf.u8(ATOM_EXT);
                // The name is ≤ 255 chars and Latin-1 is one byte per char, so the
                // 16-bit length always fits.
                self.buf.u16be(bytes.len() as u16);
                self.buf.bytes(&bytes);
            }
            None => self.write_atom_utf8(name),
        }
    }

    /// Encodes a cons list: a proper list of bytes (length ≤ 65535) becomes
    /// `STRING_EXT`, everything else (including an improper list) becomes
    /// `LIST_EXT`.
    fn encode_list(&mut self, term: Eterm) -> Result<()> {
        let mut count: u32 = 0;
        let mut all_bytes = true;
        let mut cursor = term;
        // Walk the spine once to measure it and decide the representation.
        let proper = loop {
            if is_nil(cursor) {
                break true;
            }
            if !is_list(cursor) {
                // An improper list: the tail is a non-nil, non-cons term.
                break false;
            }
            let head = cons_head(cursor)?;
            if !(is_small(head) && (0..=255).contains(&signed_value(head))) {
                all_bytes = false;
            }
            count = count.checked_add(1).ok_or_else(error::system_limit)?;
            cursor = cons_tail(cursor)?;
        };

        if proper && all_bytes && count <= u32::from(u16::MAX) {
            self.buf.u8(STRING_EXT);
            self.buf.u16be(count as u16);
            // Second walk: emit the (byte-valued) elements.
            let mut cursor = term;
            for _ in 0..count {
                let head = cons_head(cursor)?;
                self.buf.u8(signed_value(head) as u8);
                cursor = cons_tail(cursor)?;
            }
            return Ok(());
        }

        self.buf.u8(LIST_EXT);
        self.buf.u32be(count);
        let mut cursor = term;
        for _ in 0..count {
            let head = cons_head(cursor)?;
            self.encode(head)?;
            cursor = cons_tail(cursor)?;
        }
        // The tail term: `NIL_EXT` for a proper list, otherwise the improper tail.
        self.encode(cursor)?;
        Ok(())
    }

    /// Encodes a tuple: `SMALL_TUPLE_EXT` up to arity 255, else `LARGE_TUPLE_EXT`.
    fn encode_tuple(&mut self, term: Eterm, arity: usize) -> Result<()> {
        if arity <= 255 {
            self.buf.u8(SMALL_TUPLE_EXT);
            self.buf.u8(arity as u8);
        } else {
            if arity > u32::MAX as usize {
                return Err(error::system_limit());
            }
            self.buf.u8(LARGE_TUPLE_EXT);
            self.buf.u32be(arity as u32);
        }
        for index in 0..arity {
            let element = tuple_element(term, index)?;
            self.encode(element)?;
        }
        Ok(())
    }

    /// Encodes a map as `MAP_EXT` from its `(keys, values)` storage slices.
    ///
    /// The flat-map path's emitter, whose pairs arrive as two parallel slices.
    ///
    /// The pair order is emitted **as stored** (interleaved `K1,V1,…,Kn,Vn`): the codec
    /// preserves the map's order and deliberately does **not** sort or dedupe it. Order is
    /// the *constructor's* job, applied before the term reaches the codec — ERTS stores a
    /// ≤ 32-pair map sorted in `erts_cmp_flatmap_keys` order (`erl_utils.h:265-272`) and
    /// the encoder walks that stored order (`external.c:3746-3769`, `ENC_MAP_PAIR`, no
    /// sort without the `deterministic` dflag), so sorting here would corrupt a
    /// decode→re-encode round-trip. See the crate `CONTEXT.md`, §"Map key encode order".
    /// The pair count must fit the flatmap `arityval` (24-bit), else `SystemLimit`.
    ///
    /// With [`Encoder::set_deterministic`] the pairs are instead collected and sorted by
    /// full term order before emitting (the C `DFLAG_DETERMINISTIC` flat-map path,
    /// `external.c:3757-3764`).
    fn encode_map(&mut self, keys: &[Eterm], values: &[Eterm]) -> Result<()> {
        debug_assert_eq!(keys.len(), values.len(), "a flatmap's keys and values are paired");
        if self.deterministic {
            let pairs: Vec<(Eterm, Eterm)> =
                keys.iter().copied().zip(values.iter().copied()).collect();
            return self.encode_pairs_sorted_by_key(&pairs);
        }
        self.encode_map_header(keys.len())?;
        for i in 0..keys.len() {
            self.encode(keys[i])?;
            self.encode(values[i])?;
        }
        Ok(())
    }
    /// Encodes a **hash map** (> 32 pairs, a HAMT) as `MAP_EXT`, in the C encoder's wire
    /// order.
    ///
    /// [`map_pairs_storage_order`] returns the HAMT's pairs in **ascending node-slot
    /// pre-order** (its slot walk iterates slots `0..sz`, descending depth-first at each
    /// slot). The C encoder's non-`deterministic` HAMT branch (`external.c:3745-3823`)
    /// instead pushes each node's slots onto a **LIFO** work stack
    /// (`WSTACK_FAST_PUSH(ENC_HASHMAP_NODE)`/`WSTACK_FAST_PUSH(*ptr++)`) and pops them
    /// (`WSTACK_POP` is a pre-decrement pop), so it visits slots in **descending** index
    /// order — the exact reverse of the ascending pre-order. The collision-node tuple takes
    /// the same path (`external.c:3415-3427`), so a single reversal of the flattened pair
    /// list reproduces the oracle's bytes.
    ///
    /// A flat map is a single leaf run and is **not** reversed: the C emits it in stored
    /// order (`external.c:3391-3401`/`3745-3769`), which [`encode_map`](Self::encode_map)
    /// preserves. The pair count must fit the `arityval` 24-bit field, else `SystemLimit`.
    ///
    /// With [`Encoder::set_deterministic`] the reversal does not apply: the C collects
    /// the pairs into an array and sorts them by key in full term order (`external.c:3783-3807`),
    /// so the stored traversal order is discarded entirely.
    fn encode_hashmap_pairs(&mut self, pairs: &[(Eterm, Eterm)]) -> Result<()> {
        if self.deterministic {
            return self.encode_pairs_sorted_by_key(pairs);
        }
        self.encode_map_header(pairs.len())?;
        for (key, value) in pairs.iter().rev() {
            self.encode(*key)?;
            self.encode(*value)?;
        }
        Ok(())
    }
    /// Writes the `MAP_EXT` tag and the big-endian pair count `n` (the shared map
    /// prologue). `n > MAX_ARITYVAL` cannot be represented by the `arityval` header, so
    /// it is a `SystemLimit`.
    fn encode_map_header(&mut self, n: usize) -> Result<()> {
        if n > MAX_ARITYVAL {
            return Err(error::system_limit());
        }
        self.buf.u8(MAP_EXT);
        self.buf.u32be(n as u32);
        Ok(())
    }

    /// Encodes an external pid as `NEW_PID_EXT` (the modern form).
    fn encode_pid(&mut self, node: Eterm, number: u32, serial: u32, creation: u32) -> Result<()> {
        self.buf.u8(NEW_PID_EXT);
        self.encode(node)?;
        self.buf.u32be(number);
        self.buf.u32be(serial);
        self.buf.u32be(creation);
        Ok(())
    }

    /// Encodes an external port as `V4_PORT_EXT` — the form the C encoder **always**
    /// emits for a port (`external.c:3675`/`:3684`); `NEW_PORT_EXT` (`89`) is
    /// decode-only. The 64-bit id is written in full, so no port id is ever a
    /// `SystemLimit` and no id is ever truncated.
    fn encode_port(&mut self, node: Eterm, number: u64, creation: u32) -> Result<()> {
        self.buf.u8(V4_PORT_EXT);
        self.encode(node)?;
        self.buf.u64be(number);
        self.buf.u32be(creation);
        Ok(())
    }

    /// Encodes an internal pid as `NEW_PID_EXT`: `[node][number:u32 BE][serial:u32 BE][creation:u32 BE]`.
    fn encode_internal_pid(
        &mut self,
        node: Atom,
        number: u64,
        serial: u64,
        creation: u32,
    ) -> Result<()> {
        let number = u32::try_from(number).map_err(|_| error::system_limit())?;
        let serial = u32::try_from(serial).map_err(|_| error::system_limit())?;
        self.buf.u8(NEW_PID_EXT);
        self.encode(make_atom(node))?;
        self.buf.u32be(number);
        self.buf.u32be(serial);
        self.buf.u32be(creation);
        Ok(())
    }

    /// Encodes an internal port as `V4_PORT_EXT`: `[node][number:u64 BE][creation:u32 BE]`
    /// (the C encoder always emits the v4 form for a port).
    fn encode_internal_port(&mut self, node: Atom, number: u64, creation: u32) -> Result<()> {
        self.buf.u8(V4_PORT_EXT);
        self.encode(make_atom(node))?;
        self.buf.u64be(number);
        self.buf.u32be(creation);
        Ok(())
    }

    /// Encodes an internal reference as `NEWER_REFERENCE_EXT` (tag 90):
    /// `[90][len = 3: u16 BE][node atom][creation: u32 BE][n0,n1,n2: 3 × u32 BE]`.
    ///
    /// The M1 internal-reference thing carries its whole identity as one 60-bit `id`
    /// in a single word, while the wire carries the C's three 32-bit numbers;
    /// [`crate::ref_wire::ref_id_to_numbers`] is the *single* place that mapping is
    /// written (the decoder's local-node demotion goes through the same module), and
    /// it keeps `numbers[0]` below `MAX_REFERENCE` so the emitted local reference is
    /// oracle-valid.
    ///
    /// Delegating to [`Encoder::encode_ref`] keeps the `NEWER_REFERENCE_EXT` byte
    /// layout in one place — the tag, the count, the node atom and the creation word
    /// are written exactly as for an external reference, with the count fixed at the
    /// three numbers [`crate::ref_wire::ref_id_to_numbers`] returns.
    fn encode_internal_ref(&mut self, node: Atom, id: u64, creation: u32) -> Result<()> {
        let numbers = crate::ref_wire::ref_id_to_numbers(id);
        self.encode_ref(make_atom(node), &numbers, creation)
    }

    /// Encodes an external reference as `NEWER_REFERENCE_EXT` (the modern form).
    fn encode_ref(&mut self, node: Eterm, ids: &[u32], creation: u32) -> Result<()> {
        if ids.len() > usize::from(u16::MAX) {
            return Err(error::system_limit());
        }
        self.buf.u8(NEWER_REFERENCE_EXT);
        self.buf.u16be(ids.len() as u16);
        self.encode(node)?;
        self.buf.u32be(creation);
        for &id in ids {
            self.buf.u32be(id);
        }
        Ok(())
    }

    /// Encodes a bitstring: `BINARY_EXT` when its size is a whole number of bytes,
    /// `BIT_BINARY_EXT` with a trailing-bit count otherwise
    /// (`external.c:3855-3862`: `encoding = TAIL_BITS(size) ? BIT_BINARY_EXT :
    /// BINARY_EXT`).
    ///
    /// `bytes` is the base byte view, `offset_bits` the bit offset of the first
    /// significant bit inside it and `size_bits` the bit count (see
    /// [`crate::term::bitstring_bytes`]). The payload is repacked so the significant
    /// bits start at bit 0 of the output and the unused trailing bits of the last
    /// byte are **zero** — a sub-bitstring source may carry non-zero bits there
    /// (`external.c:3914-3922`).
    ///
    /// The wire length field is a 32-bit byte count, so a bitstring that cannot be
    /// represented by these two tags is a `BeamError::SystemLimit` rather than a
    /// truncated length (the C computes the size with a `Uint` and has no such
    /// check; the bound is also what keeps the repack buffer finite).
    fn encode_bitstring(
        &mut self,
        bytes: &[u8],
        offset_bits: usize,
        size_bits: usize,
    ) -> Result<()> {
        let n_bytes = bit_count_to_bytes(size_bits);
        let tail = size_bits % 8;
        let len_field = u32::try_from(n_bytes).map_err(|_| error::system_limit())?;
        if tail == 0 {
            self.buf.u8(BINARY_EXT);
            self.buf.u32be(len_field);
        } else {
            self.buf.u8(BIT_BINARY_EXT);
            self.buf.u32be(len_field);
            // `bits` is the number of significant bits in the last byte (`1..=8`).
            self.buf.u8(tail as u8);
        }
        let payload = repack_bits(bytes, offset_bits, size_bits, n_bytes)?;
        self.buf.bytes(&payload);
        Ok(())
    }

    /// The bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.as_slice()
    }

    /// The number of bytes written.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Consumes the encoder, returning the written bytes.
    pub fn finish(self) -> Vec<u8> {
        self.buf.into_vec()
    }

    /// Consumes the encoder, producing a `COMPRESSED` (`80`) `term_to_binary` image
    /// at zlib `level` (`0..=9`) — **or the plain image when compression does not
    /// help**.
    ///
    /// The written term is the *inner body*: the bytes the encoder produced, minus a
    /// leading [`VERSION_MAGIC`](crate::VERSION_MAGIC) if [`Encoder::new`] wrote one
    /// (a term body never starts with `131`, which is not a tag, so stripping it when
    /// present is unambiguous).
    ///
    /// When the zlib stream is small enough that the wrapper still shrinks the image
    /// the result is `[VERSION_MAGIC][COMPRESSED_EXT][inner_size:4 BE][zlib body]`
    /// (`external.c:2289-2297`). The wrapper costs 5 bytes (`COMPRESSED_EXT` + the
    /// 4-byte size), so — exactly as the C does with a `dest_len` of `body_len - 5`
    /// (`external.c:2276-2298`) — the wrapper is emitted only when the stream fits in
    /// `body_len - 5` bytes; otherwise the **plain** image
    /// `[VERSION_MAGIC][body]` is returned instead (compression was requested but is
    /// not profitable, and `erlang:term_to_binary/2` yields the uncompressed image
    /// then). A body shorter than 5 bytes can never fit the wrapper and always
    /// reverts.
    ///
    /// (The deflate bytes themselves are implementation-defined: zlib and
    /// `miniz_oxide` emit different, equally valid streams for the same body.)
    ///
    /// A `level` outside `0..=9` yields `BeamError::BadArg`; an inner body larger
    /// than the 32-bit size field yields `BeamError::SystemLimit`; a zlib failure
    /// would yield `BeamError::BadArg` (it cannot occur for a valid level here).
    pub fn finish_compressed(self, level: u8) -> Result<Vec<u8>> {
        crate::compress::reject_bad_level(level)?;
        let buf = self.buf.into_vec();
        let body: &[u8] = buf.strip_prefix(&[crate::VERSION_MAGIC]).unwrap_or(&buf);
        let size = u32::try_from(body.len()).map_err(|_| error::system_limit())?;

        // The C compresses into a `body_len - 5`-byte destination and keeps the
        // plain image when that overflows (`external.c:2276-2298`): the 5 bytes are
        // the `COMPRESSED_EXT` tag plus the 4-byte size field the wrapper adds.
        match compress_if_smaller(body, level)? {
            Some(stream) => {
                let mut out = Writer::with_capacity(body.len() + 6);
                out.u8(crate::VERSION_MAGIC);
                out.u8(COMPRESSED_EXT);
                out.u32be(size);
                out.bytes(&stream);
                Ok(out.into_vec())
            }
            None => {
                // Reverted to the plain image: `[VERSION_MAGIC][body]`.
                let mut out = Writer::with_capacity(body.len() + 1);
                out.u8(crate::VERSION_MAGIC);
                out.bytes(body);
                Ok(out.into_vec())
            }
        }
    }

    /// Consumes the encoder, wrapping the written term in a `LOCAL_EXT` (`121`)
    /// `term_to_binary` image at `hash`.
    ///
    /// The result is `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE][body]`, where `body` is
    /// the bytes the encoder produced minus a leading
    /// [`VERSION_MAGIC`](crate::VERSION_MAGIC) if [`Encoder::new`] wrote one (a term
    /// body never starts with `131`, which is not a tag, so stripping it when present
    /// is unambiguous — the same normalization [`Encoder::finish_compressed`]
    /// performs). This is the wire layout `erlang:term_to_binary(Term, [local])`
    /// produces: the C writes the tag plus four reserved bytes up front
    /// (`external.c:3305-3315`) and patches the 4-byte hash at the end
    /// (`external.c:4046-4105`).
    ///
    /// **The 4-byte `hash` is supplied by the caller.** The C derives it as the
    /// Bob-Jenkins block hash of the term-body bytes seeded with the node's
    /// `local_node_hash`; that node identity (hostname / pid / creation) is owned by
    /// `beam_rt`/`beam_dist`, not by this codec, so the codec neither computes nor
    /// verifies the hash — it writes the caller's value verbatim. A `LOCAL_EXT` term
    /// is therefore only meaningful to a `binary_to_term` on the same node.
    ///
    /// This is **not** the local-fun path: a local (`make_fun3`) fun has no wire form
    /// at all and still yields [`crate::error::unsupported_local_fun`].
    pub fn finish_local(self, hash: u32) -> Vec<u8> {
        let buf = self.buf.into_vec();
        let body: &[u8] = buf.strip_prefix(&[crate::VERSION_MAGIC]).unwrap_or(&buf);

        let mut out = Writer::with_capacity(body.len() + 6);
        out.u8(crate::VERSION_MAGIC);
        out.u8(LOCAL_EXT);
        out.u32be(hash);
        out.bytes(body);
        out.into_vec()
    }
}

/// Renders `ErtsDigit`s as base-256 magnitude bytes, least significant first —
/// exactly C `big_to_bytes` (`big.c:3467-3490`): every digit but the most
/// significant one contributes all `size_of::<u64>()` bytes, and the most
/// significant digit contributes only its significant bytes (at least one, so an
/// all-zero digit vector still yields a one-byte magnitude).
fn digits_to_le_bytes(digits: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(digits.len() * core::mem::size_of::<u64>());
    let Some((&most_significant, rest)) = digits.split_last() else {
        return out;
    };
    for &digit in rest {
        out.extend_from_slice(&digit.to_le_bytes());
    }
    let mut digit = most_significant;
    loop {
        out.push((digit & 0xFF) as u8);
        digit >>= 8;
        if digit == 0 {
            return out;
        }
    }
}

/// Strips the high-order zero bytes of a base-256 little-endian magnitude.
fn strip_high_zero_bytes(digits: &[u8]) -> &[u8] {
    let mut end = digits.len();
    while end > 0 && digits[end - 1] == 0 {
        end -= 1;
    }
    &digits[..end]
}

/// The Latin-1 (one byte per code point) image of `name`, or `None` when any code
/// point is outside `U+0000..=U+00FF`.
///
/// Mirrors the C's precomputed `a->latin1_chars < 0` test (`external.c:2910`): a
/// name is "Latin-1-representable" iff every character fits one ISO-8859-1 byte.
fn latin1_bytes(name: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(name.len());
    for ch in name.chars() {
        let code = ch as u32;
        if code > 0xFF {
            return None;
        }
        bytes.push(code as u8);
    }
    Some(bytes)
}

/// zlib-compresses `body`, returning the stream **only when the 5-byte
/// `COMPRESSED` wrapper still shrinks the image** — the destination-capacity rule
/// the C applies (`external.c:2276-2298`).
///
/// The C compresses into a buffer of `body_len - 5` bytes (`0` when `body_len < 5`)
/// and keeps the plain image when `compress2` reports `Z_BUF_ERROR`. Equivalently:
/// the wrapper is used only when `stream.len() + 5 <= body_len`, i.e.
/// `stream.len() <= body_len - 5`. A body shorter than 5 bytes can never satisfy
/// this (a non-empty zlib stream is always longer than 0), so it always reverts.
///
/// A `level` outside `0..=9` never reaches here (the caller rejected it); any zlib
/// failure propagates as `BeamError::BadArg`.
fn compress_if_smaller(body: &[u8], level: u8) -> Result<Option<Vec<u8>>> {
    let Some(cap) = body.len().checked_sub(5) else {
        return Ok(None);
    };
    let stream = crate::compress::compress(body, level)?;
    Ok(if stream.len() <= cap { Some(stream) } else { None })
}
/// Repacks `size_bits` significant bits — MSB-first inside each byte — from bit
/// `offset_bits` of `bytes` into exactly `n_bytes = ceil(size_bits / 8)` output
/// bytes, with the significant bits starting at bit 0 and the unused trailing bits
/// of the last byte zeroed.
///
/// This is C `copy_binary_to_buffer(dst, 0, base, offset, size)`
/// (`erl_bits.h:541-556`) together with the `ep[wire_size - 1] = 0` pre-clear the
/// `BIT_BINARY_EXT` encoder does (`external.c:3914-3922`): the destination is
/// zeroed up front, then the `size_bits` bits are copied in.
///
/// `offset_bits` is `0` for a heap bitstring (and for the byte-aligned sub-bitstring
/// case a plain `copy_from_slice` is used); a non-byte-aligned offset — only
/// reachable from a sub-bitstring — falls back to a per-bit shift. A source range
/// outside `bytes` yields `BeamError::BadArg` rather than a panic (a well-formed
/// bitstring never triggers it).
fn repack_bits(
    bytes: &[u8],
    offset_bits: usize,
    size_bits: usize,
    n_bytes: usize,
) -> Result<Vec<u8>> {
    debug_assert_eq!(
        n_bytes,
        bit_count_to_bytes(size_bits),
        "the caller sized the buffer from the bit count"
    );
    let mut out = vec![0u8; n_bytes];
    if size_bits == 0 {
        return Ok(out);
    }
    if offset_bits % 8 == 0 {
        let start = offset_bits / 8;
        let end = start.checked_add(n_bytes).ok_or_else(error::bad_arg)?;
        let src = bytes.get(start..end).ok_or_else(error::bad_arg)?;
        out.copy_from_slice(src);
    } else {
        for i in 0..size_bits {
            let bit = offset_bits + i;
            let byte = *bytes.get(bit / 8).ok_or_else(error::bad_arg)?;
            if byte & (0x80 >> (bit % 8)) != 0 {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
    }
    // The source's trailing bits are not significant: clear them.
    if size_bits % 8 != 0 {
        out[n_bytes - 1] &= 0xFFu8 << (8 - (size_bits % 8));
    }
    Ok(out)
}

/// Encodes `term` as a `term_to_binary` image: version byte plus term.
///
/// Convenience over [`Encoder::new`] + [`Encoder::encode`] + [`Encoder::finish`].
pub fn encode_term(term: Eterm, atoms: &AtomTable) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new(atoms);
    encoder.encode(term)?;
    Ok(encoder.finish())
}

/// Encodes `term` as a body (no version byte): distribution payload / ETS blob.
///
/// Convenience over [`Encoder::new_body`] + [`Encoder::encode`] +
/// [`Encoder::finish`].
pub fn encode_term_body(term: Eterm, atoms: &AtomTable) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new_body(atoms);
    encoder.encode(term)?;
    Ok(encoder.finish())
}

/// Encodes `term` as a `term_to_binary/2` image under the `{minor_version, N}`
/// flavour `minor`: version byte plus term.
///
/// Convenience over [`Encoder::new`] + [`Encoder::set_minor_version`] +
/// [`Encoder::encode`] + [`Encoder::finish`]. With [`MinorVersion::V2`] this is
/// byte-identical to [`encode_term`]; [`MinorVersion::V1`] emits Latin-1
/// `ATOM_EXT` (`100`) atoms, and [`MinorVersion::V0`] additionally emits the legacy
/// 31-byte `FLOAT_EXT` (`99`) floats (see [`MinorVersion`]).
pub fn encode_term_with_minor_version(
    term: Eterm,
    atoms: &AtomTable,
    minor: MinorVersion,
) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new(atoms).with_minor_version(minor);
    encoder.encode(term)?;
    Ok(encoder.finish())
}

/// Encodes `term` as a compressed `term_to_binary` image:
/// `[VERSION_MAGIC][COMPRESSED_EXT][inner_size:4 BE][zlib body]` — **or** the plain
/// `[VERSION_MAGIC][body]` image when compression does not shrink the term.
///
/// The term is encoded uncompressed first (the `zlib` stream is the *inner* body, so
/// it never contains the version byte), then deflated at zlib `level`. This is the
/// byte layout of `erlang:term_to_binary(Term, [{compressed, Level}])`
/// (`external.c:2289-2297`), which reverts to the uncompressed image whenever zlib
/// cannot fit the stream into `body_len - 5` bytes — the space the 5-byte wrapper
/// itself costs (`external.c:2276-2298`). The revert is applied by
/// [`Encoder::finish_compressed`], so a small term (e.g. `[1,2,3]`) yields exactly
/// what [`encode_term`] would.
///
/// The default [`encode_term`] stays uncompressed: the wrapper is emitted only here
/// (and only when it helps).
pub fn encode_term_compressed(term: Eterm, atoms: &AtomTable, level: u8) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new_body(atoms);
    encoder.encode(term)?;
    encoder.finish_compressed(level)
}

/// Encodes `term` as a compressed **body**: the same zlib wrapper as
/// [`encode_term_compressed`] but with the leading version byte omitted, i.e.
/// `[COMPRESSED_EXT][inner_size:4 BE][zlib body]` (the distribution-payload / ETS
/// blob form, where the version byte is not part of the body).
///
/// The inner bytes are the term body (as [`encode_term_body`]); the 4-byte size is
/// that body's length, version byte excluded. When compression does not shrink the
/// term, [`encode_term_compressed`] returns the plain image `[VERSION_MAGIC][body]`
/// and this entry point drops that version byte, yielding the plain `[body]` — so
/// the result is always a valid body either way.
pub fn encode_term_body_compressed(term: Eterm, atoms: &AtomTable, level: u8) -> Result<Vec<u8>> {
    let image = encode_term_compressed(term, atoms, level)?;
    // Drop the leading `VERSION_MAGIC`; everything else (tag, size, zlib stream — or
    // just the plain body) is already the body form. Both `finish_compressed` forms
    // start with the version byte, which a term body never does.
    Ok(image[1..].to_vec())
}

/// Encodes `term` as a `LOCAL_EXT` (`121`) `term_to_binary` image:
/// `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE][body]`.
///
/// The body is [`encode_term_body`]'s bytes; the wrapper only prepends the version
/// byte, the `LOCAL_EXT` tag and the caller-supplied `hash` (see
/// [`Encoder::finish_local`] — node identity, hence the hash, is owned by the
/// caller). Convenience over [`Encoder::new_body`] + [`Encoder::encode`] +
/// [`Encoder::finish_local`], mirroring [`encode_term_compressed`].
pub fn encode_term_local(term: Eterm, atoms: &AtomTable, hash: u32) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new_body(atoms);
    encoder.encode(term)?;
    Ok(encoder.finish_local(hash))
}

#[cfg(test)]
mod tests;
