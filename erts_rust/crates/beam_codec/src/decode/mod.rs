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

//! The `Decoder` seam: external term format bytes → terms.
//!
//! Decoding is **allocation-aware**: the decoded term is materialized in the
//! caller-supplied [`beam_alloc::Heap`] (production heap, not the `beam_term`
//! test arena) and every atom is interned into the caller's [`AtomTable`] in wire
//! order. The same code path therefore serves `binary_to_term`, distribution
//! payloads and ETS blobs.
//!
//! This module is safe Rust; the raw heap writes it needs live in
//! [`crate::term`]. The fun/export tag arms' helpers live in the child [`fun`]
//! module and the native-record arm in the child [`record`] module, each as a second
//! `impl` block on [`Decoder`].
//!
//! ## Nesting depth is bounded (no abort on hostile input)
//!
//! [`Decoder::decode`] is recursive, so a deeply nested image could otherwise
//! exhaust the native stack and **abort the process** (a Rust stack overflow, not a
//! typed error). `decode` therefore refuses any term nested deeper than
//! [`MAX_DECODE_DEPTH`] with `BeamError::SystemLimit`, and only *nesting* is bounded:
//! a long (wide) list or tuple decodes iteratively and is unaffected. This is a
//! deliberate, documented divergence from C `dec_term`
//! (`../../erts/emulator/beam/external.c`), which has no explicit depth limit and
//! relies on the emulator thread's stack.

mod fun;
mod local_node;
mod record;

use core::mem::size_of;
use core::str;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    is_atom, make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data,
    make_small, try_make_small, MAX_ARITYVAL, MAX_SMALL, MIN_SMALL, NIL,
};
use beam_term::refc::ERL_ONHEAP_BITS_LIMIT;
use beam_term::{Atom, AtomTable, Eterm};

use crate::atom_cache::AtomCache;
use crate::error::{bad_arg, system_limit};
use crate::off_heap::OffHeapBinaries;
use crate::reader::Reader;
use crate::tag::{Tag, COMPRESSED_EXT, VERSION_MAGIC};

/// The OTP atom-length ceiling, in **characters** (`erts/emulator/beam/erl_term.h`):
/// a longer name is a `SystemLimit`, never a truncated atom.
const MAX_ATOM_CHARS: usize = 255;

/// `BIG_ARITY_MAX` (`erl_term.h:465-469`): the largest bignum digit count.
const BIG_ARITY_MAX: usize = (1 << 16) - 1;

/// `sizeof(ErtsDigit)` (`big.h:29`): one term word on `ARCH_64`. `LARGE_BIG_EXT`'s
/// wire bound is expressed in *bytes* (`n > BIG_ARITY_MAX*sizeof(ErtsDigit)`,
/// `external.c:6146`).
const BIG_DIGIT_BYTES: usize = size_of::<u64>();

/// `MAX_REFERENCE` (`erl_term.h:771`): `1 << _REF_NUM_SIZE` = `1 << 18`. The first
/// reference number of the legacy 18-bit forms (`REFERENCE_EXT` and
/// `NEW_REFERENCE_EXT`) must stay below it (`external.c:4758` / `:4782`); the
/// modern `NEWER_REFERENCE_EXT` (and a local node in the shared block,
/// `external.c:4810`) enforces it only for its own first number.
const MAX_REFERENCE: u32 = 1 << 18;

/// The maximum **nesting depth** [`Decoder::decode`] will recurse to.
///
/// `decode` is recursive: a tuple/list/map element, a map key/value, the node atom
/// of an external identifier, a fun/record sub-term and the `LOCAL_EXT` wrapper all
/// re-enter it. Without a bound, a deeply nested image (e.g. ~55 KB of nested
/// 1-tuples) exhausts the native stack and Rust **aborts the process** with a stack
/// overflow — hostile input turned into a crash, never a typed error. This bound
/// converts that into a `BeamError::SystemLimit`.
///
/// This is a **documented divergence** from C `dec_term`
/// (`../../erts/emulator/beam/external.c`), which has no explicit depth limit: it
/// recurses on the emulator thread's stack. A bounded depth is the deliberate
/// choice here — a typed resource error is strictly better than an abort.
///
/// The value is measurement-driven. Each nesting level costs roughly 340 bytes of
/// native stack in the dev profile; on a **2 MiB** thread stack (the Rust default
/// for spawned threads, and four times *smaller* than the 8 MiB main thread) the
/// recursion first hits the guard page — and aborts — at a nesting depth of about
/// **6250** (measured: a nested-1-tuple image decodes at depth 6200 and aborts at
/// 6300). The limit is set to 1/4 of that measured cliff so the guard trips long
/// before the stack is anywhere near exhaustion, leaving room for the deeper
/// per-level frames of a debug build or a future code change.
///
/// Only **depth** is bounded, never length: the list spine and a tuple's elements
/// are decoded iteratively (a loop feeding a `Vec`), so a 1,000,000-element list
/// and a 100,000-element tuple still decode — this constant bounds nesting alone.
const MAX_DECODE_DEPTH: usize = 1500;

/// Incremental external term format decoder.
///
/// `new` requires and consumes the leading version byte `131`; `new_body`
/// decodes a body that has none (the distribution payload following a
/// distribution header, or an ETS term blob).
pub struct Decoder<'a> {
    /// The wire cursor.
    reader: Reader<'a>,
    /// The distribution atom cache, when an `ATOM_CACHE_REF` may appear.
    cache: Option<&'a AtomCache>,
    /// The off-heap binary host, when a large binary's payload should live off the
    /// process heap (see [`Decoder::with_off_heap`]).
    off_heap: Option<&'a mut dyn OffHeapBinaries>,
    /// The local node's identity — its `sysname` atom and `creation`
    /// (`erts_this_node`) — when the caller installed it with
    /// [`Decoder::with_local_node`]. `None` (the default) disables every
    /// identity-gated rule, so decoding is unchanged (see [`local_node`]).
    local_node: Option<(Atom, u32)>,
    /// The expected `LOCAL_EXT` (`121`) node hash, when the caller installed it
    /// with [`Decoder::with_local_node_hash`]. `None` (the default) leaves the
    /// 4-byte hash consumed-but-unchecked, exactly as before.
    local_hash: Option<u32>,
    /// The current nesting depth of [`Decoder::decode`], bounded by
    /// [`MAX_DECODE_DEPTH`]. Incremented on entry and decremented on **every** exit
    /// path (see `decode`), so it tracks the live recursion depth, not the number of
    /// terms decoded.
    depth: usize,
}

impl<'a> Decoder<'a> {
    /// Creates a decoder for a `term_to_binary` image.
    ///
    /// Requires and consumes the leading [`VERSION_MAGIC`](crate::VERSION_MAGIC)
    /// byte; a missing or mismatched version byte yields `BeamError::BadArg`.
    pub fn new(bytes: &'a [u8]) -> Result<Decoder<'a>> {
        let mut reader = Reader::new(bytes);
        if reader.u8()? != VERSION_MAGIC {
            return Err(bad_arg());
        }
        Ok(Decoder {
            reader,
            cache: None,
            off_heap: None,
            local_node: None,
            local_hash: None,
            depth: 0,
        })
    }

    /// Creates a decoder for a body: **no** leading version byte.
    pub fn new_body(bytes: &'a [u8]) -> Decoder<'a> {
        Decoder {
            reader: Reader::new(bytes),
            cache: None,
            off_heap: None,
            local_node: None,
            local_hash: None,
            depth: 0,
        }
    }

    /// Attaches the distribution atom cache used to resolve `ATOM_CACHE_REF` (`82`).
    pub fn with_atom_cache(self, cache: &'a AtomCache) -> Decoder<'a> {
        Decoder { cache: Some(cache), ..self }
    }

    /// Attaches the [`OffHeapBinaries`] host that owns the payload of a **large**
    /// decoded binary.
    ///
    /// With a host installed, a `BINARY_EXT`/`BIT_BINARY_EXT` whose size exceeds
    /// [`ERL_ONHEAP_BITS_LIMIT`] (512 bits, i.e. 64 bytes) is materialized as the
    /// off-heap `[BinRef][ErlSubBits]` pair — the C's `refc_binary` path — the payload
    /// coming from the host (see [`OffHeapBinaries::alloc_refc_payload`]). Without a
    /// host (the default) every bitstring stays on-heap, exactly as before the seam
    /// existed. The same on-heap/off-heap split holds for `BIT_BINARY_EXT`, whose size
    /// in bits may exceed the limit while its byte count does not.
    ///
    /// The decoded term is observably identical either way:
    /// [`crate::term::bitstring_bytes`]/[`crate::term::bitstring_size_bits`] read both
    /// representations, so `byte_size/1`, the contents and the ETF image cannot differ.
    pub fn with_off_heap(self, host: &'a mut dyn OffHeapBinaries) -> Decoder<'a> {
        Decoder { off_heap: Some(host), ..self }
    }

    /// Decodes the next term into `heap`, interning its atoms into `atoms`.
    ///
    /// Malformed/truncated input, a bad tag, an obsolete tag the C also rejects
    /// ([`Tag::Fun`], [`Tag::NewCache`]), a [`Tag::DistHeader`] (a distribution
    /// transport wrapper, never a term), invalid UTF-8 in an atom name or an unset
    /// atom-cache slot yield `BeamError::BadArg`; a value beyond a wire bound yields
    /// `BeamError::SystemLimit`; heap exhaustion propagates from the heap as
    /// `BeamError::Alloc`. Every recognized tag is implemented, so this decoder never
    /// reports [`crate::error::unsupported`].
    ///
    /// [`Tag::Local`] is **not a term tag**: it is the `term_to_binary/2 [local]`
    /// wrapper `[VERSION_MAGIC][LOCAL_EXT][node-hash:4 BE][<term>]`. As the C does
    /// (`external.c:5250-5254`), the 4-byte node hash is consumed and the *following*
    /// term is decoded — whether the wrapper is the outermost object or appears
    /// nested.
    ///
    /// Funs decode for real: [`Tag::Export`] becomes
    /// a [`FunKind::External`](crate::FunKind) fun and [`Tag::NewFun`] a
    /// [`FunKind::Imported`](crate::FunKind) one (see [`Decoder::decode_export`] and
    /// [`Decoder::decode_new_fun`]).
    ///
    /// **Nesting is bounded.** `decode` recurses on every nesting level (a
    /// tuple/list/map element, a map key/value, an identifier's node atom, a
    /// fun/record sub-term, the `LOCAL_EXT` wrapper), so a deeply nested image is
    /// refused with `BeamError::SystemLimit` once the depth reaches
    /// [`MAX_DECODE_DEPTH`] — a typed resource error instead of the native **stack
    /// overflow / process abort** an unbounded recursion would cause. Only *depth* is
    /// bounded: a wide (long) list or tuple is decoded iteratively and is never
    /// affected (see [`MAX_DECODE_DEPTH`]).
    pub fn decode(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
        // Bound the recursion *before* descending a level, so an over-deep image
        // returns a typed error rather than exhausting the native stack. C
        // `dec_term` has no such limit (it relies on the emulator thread's stack);
        // the bounded divergence is deliberate and documented at the constant.
        if self.depth >= MAX_DECODE_DEPTH {
            return Err(system_limit());
        }
        self.depth += 1;
        // The decrement runs on *every* exit path because the inner result is
        // captured before returning (the recursive arms, the `LOCAL_EXT` wrapper's
        // early `self.decode(...)`, and any `?` inside `decode_inner` all land here).
        // `decode_inner` never panics, so the counter cannot leak.
        let result = self.decode_inner(heap, atoms);
        self.depth -= 1;
        result
    }

    /// The recursive term walker behind [`Decoder::decode`], which owns the depth
    /// accounting. Kept as a separate function so the guard has exactly one
    /// increment and one decrement that no early `return` inside the walker can skip.
    fn decode_inner(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
        let byte = self.reader.u8()?;
        let tag = Tag::from_byte(byte).ok_or_else(bad_arg)?;
        match tag {
            // ---------------------------------------------------------------
            // Integers
            // ---------------------------------------------------------------
            Tag::SmallInteger => Ok(make_small(i64::from(self.reader.u8()?))),
            Tag::Integer => {
                // A 32-bit value always fits a 60-bit small.
                let value = self.reader.u32be()? as i32;
                Ok(try_make_small(i64::from(value))?)
            }
            Tag::SmallBig | Tag::LargeBig => self.decode_big(tag, heap),

            // ---------------------------------------------------------------
            // Floats
            // ---------------------------------------------------------------
            Tag::Float => {
                // 31 bytes of a NUL-terminated `%.20e` ASCII rendering.
                let raw = self.reader.bytes(31)?;
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                let text = str::from_utf8(&raw[..end]).map_err(|_| bad_arg())?;
                // `sys_chars_to_double` (`../../erts/emulator/sys/unix/sys_float.c:72-140`)
                // accepts exactly `[+-]? DIGIT+ [.,] DIGIT+ ([eE] [+-]? DIGIT+)?` and
                // nothing else, and requires a **finite** result. Rust's
                // `parse::<f64>()` is far more permissive — it also accepts `inf`,
                // `-inf`, `nan`, `infinity`, `1e5` (no radix), `1.`, `.5`, ` 1.0`,
                // hex floats and other spellings the oracle rejects with `badarg` —
                // so the grammar is validated byte by byte before parsing.
                let bytes = text.as_bytes();
                let is_digit = |i: usize| matches!(bytes.get(i), Some(&d) if d.is_ascii_digit());
                let mut i = 0usize;
                if matches!(bytes.first(), Some(&b'+') | Some(&b'-')) {
                    i += 1;
                }
                let int_start = i;
                while is_digit(i) {
                    i += 1;
                }
                if i == int_start {
                    return Err(bad_arg()); // at least one leading digit
                }
                if !matches!(bytes.get(i), Some(&b'.') | Some(&b',')) {
                    return Err(bad_arg()); // a radix character is required
                }
                i += 1;
                let frac_start = i;
                while is_digit(i) {
                    i += 1;
                }
                if i == frac_start {
                    return Err(bad_arg()); // at least one fractional digit
                }
                if matches!(bytes.get(i), Some(&b'e') | Some(&b'E')) {
                    i += 1;
                    if matches!(bytes.get(i), Some(&b'+') | Some(&b'-')) {
                        i += 1;
                    }
                    let exp_start = i;
                    while is_digit(i) {
                        i += 1;
                    }
                    if i == exp_start {
                        return Err(bad_arg()); // at least one exponent digit
                    }
                }
                if i != bytes.len() {
                    return Err(bad_arg()); // trailing characters
                }
                // The C retries `strtod` with the flipped radix character
                // (`sys_float.c:115-124`), so a `,` radix is accepted; Rust's parser
                // only knows `.`.
                let normalized;
                let parse_input = if bytes.contains(&b',') {
                    normalized = text.replace(',', ".");
                    normalized.as_str()
                } else {
                    text
                };
                let value: f64 = parse_input.parse().map_err(|_| bad_arg())?;
                // `if (!erts_isfinite(*fp)) return -1;` (`sys_float.c:111-113`).
                if !value.is_finite() {
                    return Err(bad_arg());
                }
                crate::term::make_float(heap, value)
            }
            Tag::NewFloat => {
                let bits = self.reader.u64be()?;
                // `NEW_FLOAT_EXT` cannot carry an infinity or NaN:
                // `if (!erts_isfinite(ff.fd)) goto error;` (`external.c:4667-4669`),
                // so the oracle's `binary_to_term/1` raises `badarg` for both.
                let value = f64::from_bits(bits);
                if !value.is_finite() {
                    return Err(bad_arg());
                }
                crate::term::make_float(heap, value)
            }

            // ---------------------------------------------------------------
            // Atoms
            // ---------------------------------------------------------------
            Tag::Atom => {
                let len = usize::from(self.reader.u16be()?);
                let bytes = self.reader.bytes(len)?;
                Self::atom_term(&latin1_to_string(bytes), atoms)
            }
            Tag::SmallAtom => {
                let len = usize::from(self.reader.u8()?);
                let bytes = self.reader.bytes(len)?;
                Self::atom_term(&latin1_to_string(bytes), atoms)
            }
            Tag::AtomUtf8 => {
                let len = usize::from(self.reader.u16be()?);
                let bytes = self.reader.bytes(len)?;
                Self::atom_term(str::from_utf8(bytes).map_err(|_| bad_arg())?, atoms)
            }
            Tag::SmallAtomUtf8 => {
                let len = usize::from(self.reader.u8()?);
                let bytes = self.reader.bytes(len)?;
                Self::atom_term(str::from_utf8(bytes).map_err(|_| bad_arg())?, atoms)
            }
            Tag::AtomCacheRef => {
                let index = self.reader.u8()?;
                let cache = self.cache.ok_or_else(bad_arg)?;
                let atom = cache.get(index).ok_or_else(bad_arg)?;
                Ok(make_atom(atom))
            }

            // ---------------------------------------------------------------
            // External identifiers (all carry a nested node atom)
            // ---------------------------------------------------------------
            Tag::Reference => {
                // `[101][node atom][r0:4 BE][creation:1]` — the C reads `r0`
                // **before** the 8-bit creation (`external.c:4752-4763`).
                let node = self.decode_node_atom(heap, atoms)?;
                let r0 = self.reader.u32be()?;
                // This legacy shape has only an 18-bit count space, and the C
                // rejects a first number at or past it (`external.c:4758`).
                if r0 >= MAX_REFERENCE {
                    return Err(bad_arg());
                }
                let creation = u32::from(self.reader.u8()?);
                // A legacy 8-bit creation must be *tiny* (`is_tiny_creation`,
                // `external.c:71-72` / `:4763`).
                if creation > 3 {
                    return Err(bad_arg());
                }
                // A **local node's** reference materializes as *this* runtime's internal
                // reference (`ref_ext_common`'s `dec_is_this_node` branch,
                // `external.c:4821-4862`): the single wire number is the id, zero-padded
                // to `ERTS_REF_NUMBERS`. Inert unless `Decoder::with_local_node` installed
                // the identity (see [`local_node`]).
                if self.is_local_node(node, creation) {
                    return crate::term::make_internal_ref(
                        heap,
                        crate::ref_wire::ref_numbers_to_id(&[r0]),
                    );
                }
                crate::term::make_external_ref(heap, node, &[r0], creation)
            }
            Tag::NewReference => {
                // `[114][len:2 BE][node atom][creation:1][r0:4][ids…]`. The C reads
                // the count, the node, a *tiny* 8-bit creation and then `r0`
                // **unconditionally** — never gated on `len > 0` (`external.c:4768-4784`):
                // for `len >= 1` `r0` *is* the first number (`ref_num[0] = r0`,
                // `external.c:4846`), and for `len == 0` it is read and then zeroed by
                // the odd-size padding write (`external.c:4856`). So exactly
                // `max(1, len)` id words follow the creation byte, which is what keeps
                // the stream aligned (verified against erts-17.1 through
                // `binary_to_term/2 [used]`: the 8-byte `len == 0` image without the
                // trailing word reports `used = 12`).
                let len = usize::from(self.reader.u16be()?);
                let node = self.decode_node_atom(heap, atoms)?;
                let creation = u32::from(self.reader.u8()?);
                // Tiny creation only (`external.c:4777`).
                if creation > 3 {
                    return Err(bad_arg());
                }
                let r0 = self.reader.u32be()?;
                // `r0 >= MAX_REFERENCE` ⇒ `badarg` for this form on *every* node,
                // local or remote (`external.c:4782`; verified against erts-17.1:
                // `r0 = 262143` ⇒ a ref, `r0 = 262144` ⇒ `badarg`).
                if r0 >= MAX_REFERENCE {
                    return Err(bad_arg());
                }
                // `ref_words > ERTS_MAX_REF_NUMBERS` ⇒ `badarg`
                // (`external.c:4799`, in `ref_ext_common`, i.e. *after* `r0`). The
                // count bound is a *wire* bound, and the C routes every decode
                // failure — bounds included — to `B2TBadArg` (`binary_to_term_int`,
                // `external.c:1989-1991`), so the oracle reports **`badarg`** here,
                // never `system_limit` (verified against erts-17.1).
                if len > crate::term::MAX_REF_IDS {
                    return Err(bad_arg());
                }
                // `len == 0` builds a **zero-id** external ref (the unconditional
                // `r0` word is consumed but not stored, matching the C's zeroed
                // padding write).
                let ids = if len == 0 {
                    Vec::new()
                } else {
                    let mut ids = Vec::with_capacity(len);
                    ids.push(r0);
                    ids.extend(self.read_ids(len - 1)?);
                    ids
                };
                // Local-node demotion: an internal reference (see the `Tag::Reference`
                // arm and `ref_ext_common`, `external.c:4821-4862`). The empty `len == 0`
                // slice maps to id 0 — exactly the C's zeroed padding.
                if self.is_local_node(node, creation) {
                    return crate::term::make_internal_ref(
                        heap,
                        crate::ref_wire::ref_numbers_to_id(&ids),
                    );
                }
                crate::term::make_external_ref(heap, node, &ids, creation)
            }
            Tag::NewerReference => {
                // `[90][len:2 BE][node atom][creation:4][r0:4][ids…]`
                // (`external.c:4786-4796`): a full 32-bit creation, the same
                // unconditional `r0` as `NEW_REFERENCE_EXT`, and **no** tiny-creation
                // bound — the only `r0` check the C has for `90` sits in the
                // *local*-node branch below.
                let len = usize::from(self.reader.u16be()?);
                let node = self.decode_node_atom(heap, atoms)?;
                let creation = self.reader.u32be()?;
                let r0 = self.reader.u32be()?;
                // `r0 >= MAX_REFERENCE` is rejected **only for the local node**
                // (`external.c:4804-4813`: the magic-ref table relies on 18 bits);
                // a remote image keeps any `r0` (verified against erts-17.1:
                // `r0 = 4294967295` on a remote node ⇒ a ref whose `262144`-valued
                // first number re-encodes unharmed, while the same `r0` on the local
                // node is `badarg`). Inert unless `Decoder::with_local_node` installed
                // the identity.
                self.check_local_ref_first_number(node, creation, r0)?;
                // `ERTS_MAX_REF_NUMBERS` (`erl_term.h:780`) ⇒ `badarg` (verified
                // against erts-17.1).
                if len > crate::term::MAX_REF_IDS {
                    return Err(bad_arg());
                }
                // `len == 0` builds a **zero-id** external ref; the image re-encodes
                // as the 11-byte `[90][0][node][creation]` the C emits.
                let ids = if len == 0 {
                    Vec::new()
                } else {
                    let mut ids = Vec::with_capacity(len);
                    ids.push(r0);
                    ids.extend(self.read_ids(len - 1)?);
                    ids
                };
                // Local-node demotion (see the `Tag::Reference` arm): the C's
                // `ref_ext_common` builds the internal reference from only the first
                // `ERTS_REF_NUMBERS` numbers, zero-padding a shorter image.
                if self.is_local_node(node, creation) {
                    return crate::term::make_internal_ref(
                        heap,
                        crate::ref_wire::ref_numbers_to_id(&ids),
                    );
                }
                crate::term::make_external_ref(heap, node, &ids, creation)
            }
            Tag::Port => {
                // `[PORT_EXT][node atom][id:4 BE u32][creation:1]` (`external.c:4685-4716`:
                // `get_uint32` for the id — a full 32-bit word, never 16-bit — then
                // `get_int8` for the creation; the sizing pass skips 5 extra bytes,
                // `external.c:6210-6211`).
                let node = self.decode_node_atom(heap, atoms)?;
                let number = u64::from(self.reader.u32be()?);
                let creation = u32::from(self.reader.u8()?);
                // Tiny creation only (`external.c:4709`).
                if creation > 3 {
                    return Err(bad_arg());
                }
                // D6: a *local* port id is a 60-bit immediate (`_PORT_NUM_SIZE`), so
                // the C rejects an id past `ERTS_MAX_INTERNAL_PORT_NUMBER`
                // (`external.c:4718-4721`). Inert without a local node identity.
                self.check_local_port(node, creation, number)?;
                // A local node's port materializes as the internal port immediate
                // (`dec_port`, `external.c:4721`), so `P =:= self()`-style identity
                // holds. Inert without a local node identity.
                if self.is_local_node(node, creation) {
                    return Ok(make_internal_port(make_port_data(number)));
                }
                crate::term::make_external_port(heap, node, number, creation)
            }
            Tag::NewPort => {
                let node = self.decode_node_atom(heap, atoms)?;
                let number = u64::from(self.reader.u32be()?);
                let creation = self.reader.u32be()?;
                self.check_local_port(node, creation, number)?;
                if self.is_local_node(node, creation) {
                    return Ok(make_internal_port(make_port_data(number)));
                }
                crate::term::make_external_port(heap, node, number, creation)
            }
            Tag::V4Port => {
                // `[V4_PORT_EXT][node atom][id:8 BE u64][creation:4 BE u32]`
                // (`external.c:4698-4716`). This is the form the C encoder *always*
                // emits for a port (`external.c:3675`/`:3684`).
                let node = self.decode_node_atom(heap, atoms)?;
                let number = self.reader.u64be()?;
                let creation = self.reader.u32be()?;
                // The only port tag whose id can exceed the 60-bit local bound.
                self.check_local_port(node, creation, number)?;
                if self.is_local_node(node, creation) {
                    return Ok(make_internal_port(make_port_data(number)));
                }
                crate::term::make_external_port(heap, node, number, creation)
            }
            Tag::Pid => {
                let node = self.decode_node_atom(heap, atoms)?;
                let number = self.reader.u32be()?;
                let serial = self.reader.u32be()?;
                let creation = u32::from(self.reader.u8()?);
                // `PID_EXT` carries an 8-bit creation that must be tiny
                // (`erlang:dec_pid`, `external.c:3159-3164` / `is_tiny_creation`
                // `external.c:71-72`).
                if creation > 3 {
                    return Err(bad_arg());
                }
                // D6: `dec_pid` bounds a *local* pid's number/serial
                // (`external.c:3175-3179`).
                self.check_local_pid(node, creation, number, serial)?;
                // A local node's pid materializes as the internal pid immediate
                // (`dec_pid`, `external.c:3175-3182`), so `P =:= self()` holds. The wire
                // reads `number` first, then `serial`; `make_pid_data` takes (serial,
                // number). Inert without a local node identity.
                if self.is_local_node(node, creation) {
                    return Ok(make_internal_pid(make_pid_data(
                        u64::from(serial),
                        u64::from(number),
                    )));
                }
                crate::term::make_external_pid(heap, node, number, serial, creation)
            }
            Tag::NewPid => {
                let node = self.decode_node_atom(heap, atoms)?;
                let number = self.reader.u32be()?;
                let serial = self.reader.u32be()?;
                let creation = self.reader.u32be()?;
                self.check_local_pid(node, creation, number, serial)?;
                if self.is_local_node(node, creation) {
                    return Ok(make_internal_pid(make_pid_data(
                        u64::from(serial),
                        u64::from(number),
                    )));
                }
                crate::term::make_external_pid(heap, node, number, serial, creation)
            }

            // ---------------------------------------------------------------
            // Tuples, lists, maps
            // ---------------------------------------------------------------
            Tag::SmallTuple | Tag::LargeTuple => {
                let arity = if tag == Tag::SmallTuple {
                    usize::from(self.reader.u8()?)
                } else {
                    let arity = self.reader.u32be()? as usize;
                    // The arity is a wire bound, and the C routes every decode failure
                    // — bounds included — to `B2TBadArg` (`binary_to_term_int`,
                    // `external.c:1989-1991`), so the oracle reports **`badarg`** for
                    // an over-long arity, never `system_limit` (verified: erts-17.1
                    // answers `badarg` for `<<131,105,1,0,0,0>>`).
                    if arity > MAX_ARITYVAL {
                        return Err(bad_arg());
                    }
                    arity
                };
                // Left empty on purpose: a length field is attacker-controlled, so
                // the allocation must not be sized from it before the bytes that
                // back the elements have been shown to exist.
                let mut elems = Vec::new();
                for _ in 0..arity {
                    elems.push(self.decode(heap, atoms)?);
                }
                heap.tuple(&elems)
            }
            Tag::Nil => Ok(NIL),
            Tag::String => {
                let len = usize::from(self.reader.u16be()?);
                let bytes = self.reader.bytes(len)?;
                // A string is a list of smalls, built back-to-front.
                let mut tail = NIL;
                for &byte in bytes.iter().rev() {
                    tail = heap.cons(make_small(i64::from(byte)), tail)?;
                }
                Ok(tail)
            }
            Tag::List => {
                let len = self.reader.u32be()? as usize;
                let mut elems = Vec::new();
                for _ in 0..len {
                    elems.push(self.decode(heap, atoms)?);
                }
                // The tail is the next term: `NIL` for a proper list, otherwise
                // the improper tail.
                let mut tail = self.decode(heap, atoms)?;
                for elem in elems.into_iter().rev() {
                    tail = heap.cons(elem, tail)?;
                }
                Ok(tail)
            }
            Tag::Map => {
                let count = self.reader.u32be()? as usize;
                // ERTS rejects a pair count with bit 31 set before reading any
                // pair: the sizing pass' `(n >> 31) != 0` guard on 64-bit
                // `ARCH_64` limits a map to `2^31 - 1` pairs
                // (`external.c:6311-6315`). A larger count is malformed.
                if (count >> 31) != 0 {
                    return Err(bad_arg());
                }
                // The pair count is attacker-controlled, so the pairs vector grows
                // as pairs are read rather than being pre-sized from `count`; a
                // count that overruns the wire fails on the first truncated read
                // (below), never with a blind length-driven allocation. The wire
                // order is interleaved `K1,V1,…,Kn,Vn`.
                let mut pairs = Vec::new();
                for _ in 0..count {
                    let key = self.decode(heap, atoms)?;
                    let value = self.decode(heap, atoms)?;
                    pairs.push((key, value));
                }
                // ERTS canonicalizes the map here (the `external.c:5438-5456`
                // fixup): `count <= MAP_SMALL_MAP_LIMIT` becomes a FLAT map with
                // keys sorted in `erts_cmp_flatmap_keys` order
                // (`erts_validate_and_sort_flatmap`, `erl_map.c:3187`), a larger
                // count a HAMT (`erts_hashmap_from_array(..., 1)`, `erl_map.c:644`,
                // keyed by `erts_map_hash`). `write_map` is that constructor, so the
                // decoded map carries ERTS's storage order and re-encodes
                // byte-identically (see the crate `CONTEXT.md`, §"Map key encode
                // order").
                let map = crate::term::make_map(heap, &pairs, atoms)?;
                // `write_map` collapses duplicate keys (last value wins), whereas
                // ERTS **rejects** them in both representations (`external.c:5448`
                // `error_map_fixup`, reached when the fixup's duplicate check
                // fails). `maps:from_list/1` is what collapses, not the codec, so a
                // produced key count below the wire count means a duplicate was
                // present and the image is malformed.
                if beam_term::map::map_size(map, atoms)? != count {
                    return Err(bad_arg());
                }
                Ok(map)
            }

            // ---------------------------------------------------------------
            // Binaries / bitstrings
            // ---------------------------------------------------------------
            Tag::Binary | Tag::BitBinary => self.decode_bitstring(tag, heap),

            // ---------------------------------------------------------------
            // `COMPRESSED` — top-level only
            // ---------------------------------------------------------------
            // The C accepts `COMPRESSED` (`80`) only as the outermost wrapper,
            // immediately after the version byte (`external.c:1759`); `decode_term`
            // unwraps it there. Reaching it here — nested inside a term, or as a
            // bare tag — is malformed input, never a term.
            Tag::Compressed => Err(bad_arg()),

            // ---------------------------------------------------------------
            // Obsolete / dead tags — malformed input, never a term
            // ---------------------------------------------------------------
            // `FUN_EXT` (`117`) was removed in OTP 23: the C decoder rejects it
            // outright (`goto error`, `external.c:6399-6404`), so it is malformed
            // (`BadArg`), not a "recognized but unimplemented" tag.
            Tag::Fun => Err(bad_arg()),
            // `NEW_CACHE` (`78`) is dead in this OTP (`#if 0`, `external.h:77-81`);
            // byte `78` is `MAGIC_REF_INTERNAL_REF`, which only ever appears inside a
            // distribution payload's internal-tag stream, never as a term tag — so in
            // a term it is malformed (`BadArg`), not `unsupported`.
            Tag::NewCache => Err(bad_arg()),

            // ---------------------------------------------------------------
            // Funs / exports
            // ---------------------------------------------------------------
            // `EXPORT_EXT` (`113`) and `NEW_FUN_EXT` (`112`) become the two
            // corresponding fun kinds; both are handled *here*, inside the shared
            // walker, so a fun nested in a tuple/list/map decodes like any other
            // sub-term (a `.beam` `LitT` literal is exactly that).
            Tag::Export => self.decode_export(heap, atoms),
            Tag::NewFun => self.decode_new_fun(heap, atoms),

            // ---------------------------------------------------------------
            // `LOCAL_EXT` — the `[local]` wrapper, not a term
            // ---------------------------------------------------------------
            // `LOCAL_EXT` (`121`) is the wrapper `term_to_binary/2` emits with the
            // `[local]` option, never a term of its own:
            // `[VERSION_MAGIC][LOCAL_EXT][node-hash:4 BE][<ordinary term>]`. The C
            // `dec_term` consumes the 4-byte node hash and continues decoding the
            // *next* object (`flags |= ERTS_DIST_EXT_INTERNAL_NC; ep += 4; goto
            // continue_this_obj;`, `external.c:5250-5254`), materializing no term for
            // the tag — so we do the same, whether the wrapper is top-level (via
            // `Decoder::new`) or nested.
            //
            // The 4-byte hash is verified only when the caller installed the expected
            // value (`Decoder::with_local_node_hash`); the C hashes the inner term's
            // bytes seeded with the running node's `local_node_hash`
            // (`external.c:6471-6512`) inside the `decoded_size` sizing pass, which
            // `binary_to_term/1,2` always runs — node identity is the caller's, so
            // with nothing installed the bytes stay consumed-but-unchecked, exactly
            // as `dec_term` does (`external.c:5250-5254`).
            Tag::Local => {
                let node_hash = self.reader.u32be()?;
                self.check_local_node_hash(node_hash)?;
                return self.decode(heap, atoms);
            }

            // ---------------------------------------------------------------
            // Malformed tags — never a term
            // ---------------------------------------------------------------
            // `DIST_HEADER` (`68`) is a distribution-transport wrapper, never a term:
            // the C term decoder has no `case DIST_HEADER` and falls into `default:
            // goto error` (`external.c:6464`, `:5383`), so `binary_to_term(<<131,68,
            // …>>)` is `badarg`. The whole `'D'`/`'E'`/`'F'` framing (dist header, atom
            // cache, fragment headers) is owned by the sibling `beam_dist`
            // (`../beam_dist/src/dist_header.rs`, `../beam_dist/src/connection.rs`);
            // none of it is duplicated here.
            Tag::DistHeader => Err(bad_arg()),

            // ---------------------------------------------------------------
            // Native records
            // ---------------------------------------------------------------
            // `RECORD_EXT` (`67`): the legacy native-record term (ordinary Erlang
            // records encode as tuples). Handled inside the shared walker, like the
            // fun family, so a record nested in a tuple/list/map decodes as a
            // sub-term; the interior block is `beam_term::record`'s.
            Tag::Record => self.decode_record(heap, atoms),
        }
    }

    /// The current cursor offset into the input.
    pub fn position(&self) -> usize {
        self.reader.position()
    }

    /// The number of bytes not yet decoded.
    pub fn remaining(&self) -> usize {
        self.reader.remaining()
    }

    /// Consumes the decoder, asserting that every byte was consumed.
    ///
    /// Trailing bytes yield `BeamError::BadArg`.
    pub fn finish(self) -> Result<()> {
        if self.reader.is_empty() {
            Ok(())
        } else {
            Err(bad_arg())
        }
    }

    /// Interns `name` and returns the atom term.
    ///
    /// A name longer than `MAX_ATOM_CHARS` *characters* is malformed input
    /// (`badarg`): C rejects such a name in `erts_atom_put`/`erts_atom_get`
    /// (`atom.c:258-274`, `atom.c:392-418`) and `dec_term` turns that into
    /// `goto error` (`external.c:4529-4539`).
    fn atom_term(name: &str, atoms: &mut AtomTable) -> Result<Eterm> {
        if name.chars().count() > MAX_ATOM_CHARS {
            return Err(bad_arg());
        }
        Ok(make_atom(atoms.intern(name)))
    }

    /// Decodes a nested term and requires it to be an atom (the node field of an
    /// external identifier).
    fn decode_node_atom(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
        let node = self.decode(heap, atoms)?;
        if !is_atom(node) {
            return Err(bad_arg());
        }
        Ok(node)
    }

    /// Reads `len` big-endian `u32` reference ids.
    fn read_ids(&mut self, len: usize) -> Result<Vec<u32>> {
        let mut ids = Vec::with_capacity(len);
        for _ in 0..len {
            ids.push(self.reader.u32be()?);
        }
        Ok(ids)
    }

    /// Decodes a `SMALL_BIG_EXT`/`LARGE_BIG_EXT` magnitude into a small integer
    /// when it fits one, otherwise into a bignum.
    ///
    /// Mirrors `external.c:4457-4497` (`dec_term`): the sign byte is `neg =
    /// get_int8(ep)` and C treats it as a boolean — **any non-zero value means
    /// negative** (`bytes_to_big`'s `if (xsgn) d = -d;`, `big.c:3446-3448`), while
    /// a zero magnitude defaults to `0` whatever the sign byte is
    /// (`external.c:4484-4486`). The magnitude is base-256 **least significant
    /// first**, and a magnitude that fits a single `ErtsDigit` and a 60-bit small
    /// decodes to a small (never a bignum). Leading zero bytes are stripped first,
    /// exactly as C does "to avoid creating illegal bignums".
    fn decode_big(&mut self, tag: Tag, heap: &mut Heap) -> Result<Eterm> {
        let byte_count = if tag == Tag::SmallBig {
            usize::from(self.reader.u8()?)
        } else {
            let count = self.reader.u32be()? as usize;
            // A byte count past `BIG_ARITY_MAX * sizeof(ErtsDigit)`
            // (`external.c:6146`) is a *wire* bound, and the C routes every decode
            // failure — bounds included — to `B2TBadArg` (`binary_to_term_int`,
            // `external.c:1989-1991`): the oracle reports **`badarg`**, never
            // `system_limit` (verified: 524281 bytes ⇒ `badarg`, 524280 ⇒ `ok`).
            if count > BIG_ARITY_MAX * BIG_DIGIT_BYTES {
                return Err(bad_arg());
            }
            count
        };
        // Any non-zero sign byte means negative (`external.c:4471` + `big.c:3446`).
        let negative = self.reader.u8()? != 0;
        let bytes = self.reader.bytes(byte_count)?;

        // Strip high-order zero bytes (the most significant end of a
        // least-significant-first slice, `external.c:4474-4482`).
        let mut end = bytes.len();
        while end > 0 && bytes[end - 1] == 0 {
            end -= 1;
        }
        let magnitude = &bytes[..end];

        if magnitude.is_empty() {
            return Ok(make_small(0));
        }
        if magnitude.len() <= BIG_DIGIT_BYTES {
            let mut value: u64 = 0;
            for (i, &byte) in magnitude.iter().enumerate() {
                value |= u64::from(byte) << (8 * i);
            }
            let signed = if negative { -(value as i128) } else { value as i128 };
            if signed >= i128::from(MIN_SMALL) && signed <= i128::from(MAX_SMALL) {
                return Ok(make_small(signed as i64));
            }
        }
        crate::term::make_big(heap, negative, &bytes_to_digits(magnitude))
    }

    /// Decodes `BINARY_EXT`/`BIT_BINARY_EXT` into a bitstring
    /// (`external.c:4893-4980`).
    ///
    /// The wire is `[len:4 BE][data]` for a binary and `[len:4 BE][bits:1][data]`
    /// for a bit binary — the length field comes **before** the trailing-bit count.
    /// `len` is attacker-controlled, so it is checked against the bytes actually
    /// left in the cursor *before* anything is allocated: a lying length is
    /// `BeamError::BadArg`, never a blind allocation. For `BIT_BINARY_EXT` the
    /// trailing-bit count must be `1..=8` and `len * 8` must be at least 8 (i.e.
    /// `len != 0`), exactly as the C, and `size_in_bits = len * 8 - (8 - bits)`.
    ///
    /// **On-heap vs off-heap.** Like the C, the size in **bits** decides the
    /// representation: at most [`ERL_ONHEAP_BITS_LIMIT`] bits becomes an on-heap
    /// `ErlHeapBits` block ([`crate::term::make_bitstring`]); more becomes the
    /// off-heap `[BinRef][ErlSubBits]` pair ([`crate::term::make_refc_binary`]) whose
    /// payload comes from the installed [`OffHeapBinaries`] host. Without a host every
    /// bitstring stays on-heap — the same observable term, so nothing regresses.
    ///
    /// The C clears the unused trailing bits of the last byte before copying; here
    /// [`crate::term::make_bitstring`] zeroes them, so the term carries no garbage
    /// from a hostile wire.
    fn decode_bitstring(&mut self, tag: Tag, heap: &mut Heap) -> Result<Eterm> {
        let len = self.reader.u32be()? as usize;
        // `BIT_BINARY_EXT` carries the number of significant bits in the last byte
        // right after the length; a plain binary is byte-aligned (a full 8).
        let trailing_bits = if tag == Tag::BitBinary {
            self.reader.u8()?
        } else {
            8
        };
        // A length past the end of the cursor is a lie: reject it rather than size an
        // allocation from it.
        if len > self.reader.remaining() {
            return Err(bad_arg());
        }
        let data = self.reader.bytes(len)?;
        // `len` is at most `u32::MAX`, so this cannot overflow on 64-bit; the guard
        // keeps the arithmetic total on every target.
        let size_in_bits = len.checked_mul(8).ok_or_else(system_limit)?;
        let size_in_bits = if tag == Tag::BitBinary {
            if !(1..=8).contains(&trailing_bits) || size_in_bits < 8 {
                return Err(bad_arg());
            }
            size_in_bits - (8 - usize::from(trailing_bits))
        } else {
            size_in_bits
        };
        match self.off_heap.as_deref_mut() {
            Some(host) if size_in_bits > ERL_ONHEAP_BITS_LIMIT => {
                crate::term::make_refc_binary(heap, host, data, 0, size_in_bits)
            }
            _ => crate::term::make_bitstring(heap, data, size_in_bits),
        }
    }
}

/// Decodes a `term_to_binary` image into `heap`, interning atoms into `atoms`.
///
/// Convenience over [`Decoder::new`] + [`Decoder::decode`] + [`Decoder::finish`].
///
/// A top-level `COMPRESSED` (`80`) wrapper is unwrapped here **before** the inner
/// term is decoded: see [`decompress_wrapper`]. (It cannot be handled inside
/// [`Decoder`] without changing its lifetimes, because the decompressed body is an
/// owned buffer the borrowed `Decoder<'a>` cannot hold.)
///
/// **Bytes after a compressed image's inner term are ignored** (the plain path keeps
/// its historical strict `finish()`). The C inflates the payload and then hands the
/// *inflated buffer* to the ordinary term decoder, which — like `binary_to_term/1,2`
/// itself — never requires that buffer to be fully consumed
/// (`binary2term_prepare`, `external.c:1744-1816`, followed by `dec_term`; the
/// `binary_to_term/2` `used` option exists precisely because trailing bytes are
/// permitted). A `finish()` on the inflated body would make a compressed image
/// *stricter* than the identical uncompressed image — an observable divergence from
/// the oracle — so the wrapper stays transparent here.
pub fn decode_term(bytes: &[u8], heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
    if let Some(body) = decompress_wrapper(bytes)? {
        let mut decoder = Decoder::new_body(&body);
        return decoder.decode(heap, atoms);
    }
    let mut decoder = Decoder::new(bytes)?;
    let term = decoder.decode(heap, atoms)?;
    decoder.finish()?;
    Ok(term)
}

/// Like [`decode_term`], but installs the local node identity so an incoming
/// identifier whose node/creation name the running node is materialized as this
/// runtime's **internal** form (internal pid/port immediate, M1 internal
/// reference) instead of an external thing — the C's `dec_is_this_node` branches
/// (`external.c:3175-3182` pid, `:4721` port, `:4821-4862` ref).
///
/// `node` is the `sysname` atom **as interned in `atoms`** (the same atom the wire
/// image carries for the local node) and `creation` is the running node's creation
/// — the caller (`beam_rt`, which owns node identity) resolves both. The identity
/// is installed on **both** the compressed-body path and the plain path, so a
/// `term_to_binary/2 [compressed]` image demotes exactly like its uncompressed
/// twin. When `local_hash` is `Some(h)`, a `LOCAL_EXT` (`121`) wrapper's 4-byte
/// node hash must equal `h` else `BeamError::BadArg` (the `decoded_size` check,
/// `external.c:6505-6511`); `None` consumes the hash unchecked.
///
/// [`decode_term`] itself stays identity-free: with no identity installed decoding
/// is byte-for-byte unchanged, so the distribution payload path is untouched.
///
/// # Errors
///
/// Every error [`decode_term`] reports, plus `BeamError::BadArg` for a mismatched
/// `LOCAL_EXT` hash and `BeamError::SystemLimit` for a local reference id that does
/// not fit the M1 block's 60-bit slot.
pub fn decode_term_with_local_node(
    bytes: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    node: Atom,
    creation: u32,
    local_hash: Option<u32>,
) -> Result<Eterm> {
    if let Some(body) = decompress_wrapper(bytes)? {
        let mut decoder = Decoder::new_body(&body).with_local_node(node, creation);
        if let Some(hash) = local_hash {
            decoder = decoder.with_local_node_hash(hash);
        }
        return decoder.decode(heap, atoms);
    }
    let mut decoder = Decoder::new(bytes)?.with_local_node(node, creation);
    if let Some(hash) = local_hash {
        decoder = decoder.with_local_node_hash(hash);
    }
    let term = decoder.decode(heap, atoms)?;
    decoder.finish()?;
    Ok(term)
}

/// Whether `bytes` is a top-level `COMPRESSED` image:
/// `[VERSION_MAGIC][COMPRESSED_EXT][size:4 BE][zlib stream]`.
///
/// The 6-byte minimum mirrors the C check (`external.c:1754-1759`): once the version
/// byte is consumed, the C requires at least the tag plus the 4-byte size before it
/// takes the `COMPRESSED` branch, so a shorter image is treated as an ordinary (and
/// therefore invalid) term starting at the `80` byte.
fn is_compressed_image(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && bytes[0] == VERSION_MAGIC && bytes[1] == COMPRESSED_EXT
}

/// Unwraps a top-level `COMPRESSED` image, returning the inner term body.
///
/// `Ok(None)` means the image is not compressed and the caller decodes it normally;
/// `Ok(Some(body))` is the decompressed inner term — the bytes that follow the
/// version byte of the *inner* `term_to_binary` image, so they start at the term tag
/// and carry no version byte.
///
/// The 4-byte size field is the inner term's byte length **excluding** the version
/// byte; a decompressed length that disagrees with it, or any zlib failure, yields
/// `BeamError::BadArg` (`external.c:1807-1810`), never a partially decoded term.
fn decompress_wrapper(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    if !is_compressed_image(bytes) {
        return Ok(None);
    }
    // `is_compressed_image` guarantees `bytes[..6]` is present.
    let size = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]) as usize;
    // Inflate bounded by the declared size: the C allocates exactly `size` bytes and
    // requires zlib to fill it (`dlen != dest_len` is an error, `external.c:1805-1810`),
    // so a payload that inflates to more than the declaration is `badarg` too. Using the
    // declaration (rather than `decompress`'s fixed 32 MiB ceiling) keeps a legitimately
    // large payload — e.g. the 33 MiB binary `tests/compressed_oracle.rs` compresses —
    // decodable, exactly as the oracle does (`external.c:1768-1781` measures the real
    // uncompressed size instead of failing).
    let body = crate::compress::decompress_bounded(&bytes[6..], size)?;
    if body.len() != size {
        return Err(bad_arg());
    }
    Ok(Some(body))
}

/// Groups base-256 magnitude bytes (least significant first) into `ErtsDigit`s —
/// exactly C `bytes_to_big` (`big.c:3424-3462`): one little-endian digit per full
/// `sizeof(ErtsDigit)` byte group, the last group holding the remaining bytes.
fn bytes_to_digits(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks(BIG_DIGIT_BYTES)
        .map(|chunk| {
            let mut group = [0u8; BIG_DIGIT_BYTES];
            group[..chunk.len()].copy_from_slice(chunk);
            u64::from_le_bytes(group)
        })
        .collect()
}

/// Renders Latin-1 bytes as a `String`: each byte `0x00..=0xFF` is the code point
/// `U+0000..=U+00FF` (one byte is always exactly one character).
fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

#[cfg(test)]
mod tests;
