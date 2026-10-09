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

mod fun;
mod record;

use core::mem::size_of;
use core::str;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    is_atom, make_atom, make_small, try_make_small, MAX_ARITYVAL, MAX_SMALL, MIN_SMALL, NIL,
};
use beam_term::refc::ERL_ONHEAP_BITS_LIMIT;
use beam_term::{AtomTable, Eterm};

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
        Ok(Decoder { reader, cache: None, off_heap: None })
    }

    /// Creates a decoder for a body: **no** leading version byte.
    pub fn new_body(bytes: &'a [u8]) -> Decoder<'a> {
        Decoder { reader: Reader::new(bytes), cache: None, off_heap: None }
    }

    /// Attaches the distribution atom cache used to resolve `ATOM_CACHE_REF` (`82`).
    pub fn with_atom_cache(self, cache: &'a AtomCache) -> Decoder<'a> {
        Decoder { reader: self.reader, cache: Some(cache), off_heap: self.off_heap }
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
        Decoder { reader: self.reader, cache: self.cache, off_heap: Some(host) }
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
    pub fn decode(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
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
                let value: f64 = text.parse().map_err(|_| bad_arg())?;
                crate::term::make_float(heap, value)
            }
            Tag::NewFloat => {
                let bits = self.reader.u64be()?;
                crate::term::make_float(heap, f64::from_bits(bits))
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
                let node = self.decode_node_atom(heap, atoms)?;
                let id = self.reader.u32be()?;
                // `MAX_REFERENCE` (`erl_term.h:771`, `1 << _REF_NUM_SIZE` = `1 << 18`):
                // this legacy shape has only an 18-bit count space, and the C rejects
                // a first id at or past it (`external.c:4757`). The modern
                // `NEWER_REFERENCE_EXT` carries no such bound.
                if id >= 262_144u32 {
                    return Err(bad_arg());
                }
                let creation = u32::from(self.reader.u8()?);
                // A legacy 8-bit creation must be *tiny* (`is_tiny_creation`,
                // `external.c:71-72` / `:4763`).
                if creation > 3 {
                    return Err(bad_arg());
                }
                crate::term::make_external_ref(heap, node, &[id], creation)
            }
            Tag::NewReference => {
                let len = usize::from(self.reader.u16be()?);
                let node = self.decode_node_atom(heap, atoms)?;
                let creation = u32::from(self.reader.u8()?);
                // Tiny creation only (`external.c:4777`).
                if creation > 3 {
                    return Err(bad_arg());
                }
                let ids = self.read_ids(len)?;
                // The first id is 18-bit-bounded for this form too (`external.c:4782`).
                if matches!(ids.first(), Some(&id) if id >= 262_144u32) {
                    return Err(bad_arg());
                }
                crate::term::make_external_ref(heap, node, &ids, creation)
            }
            Tag::NewerReference => {
                let len = usize::from(self.reader.u16be()?);
                // `ERTS_MAX_REF_NUMBERS` (`erl_term.h:780`); `make_external_ref`
                // enforces the same bound.
                if len > crate::term::MAX_REF_IDS {
                    return Err(system_limit());
                }
                let node = self.decode_node_atom(heap, atoms)?;
                let creation = self.reader.u32be()?;
                let ids = self.read_ids(len)?;
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
                crate::term::make_external_port(heap, node, number, creation)
            }
            Tag::NewPort => {
                let node = self.decode_node_atom(heap, atoms)?;
                let number = u64::from(self.reader.u32be()?);
                let creation = self.reader.u32be()?;
                crate::term::make_external_port(heap, node, number, creation)
            }
            Tag::V4Port => {
                // `[V4_PORT_EXT][node atom][id:8 BE u64][creation:4 BE u32]`
                // (`external.c:4698-4716`). This is the form the C encoder *always*
                // emits for a port (`external.c:3675`/`:3684`).
                let node = self.decode_node_atom(heap, atoms)?;
                let number = self.reader.u64be()?;
                let creation = self.reader.u32be()?;
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
                crate::term::make_external_pid(heap, node, number, serial, creation)
            }
            Tag::NewPid => {
                let node = self.decode_node_atom(heap, atoms)?;
                let number = self.reader.u32be()?;
                let serial = self.reader.u32be()?;
                let creation = self.reader.u32be()?;
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
                    if arity > MAX_ARITYVAL {
                        return Err(system_limit());
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
                // The pair count becomes a flatmap `arityval`, whose size field is
                // `MAX_ARITYVAL` wide, so a larger map cannot be represented.
                if count > MAX_ARITYVAL {
                    return Err(system_limit());
                }
                // The wire order (interleaved `K1,V1,…,Kn,Vn`) is what defines the map,
                // so it is preserved verbatim: this crate never sorts or dedupes on
                // decode either. ERTS stores a ≤ 32-pair map sorted in
                // `erts_cmp_flatmap_keys` order and emits that order on the wire
                // (`erl_map.h:181-199`, `external.c:3746-3769`), so a faithful
                // decode→re-encode is byte-stable (see the crate `CONTEXT.md`,
                // §"Map key encode order"). Any pair count (including a > 32-pair HAMT's
                // image) is materialized as one flat map here.
                let mut pairs = Vec::new();
                for _ in 0..count {
                    let key = self.decode(heap, atoms)?;
                    let value = self.decode(heap, atoms)?;
                    pairs.push((key, value));
                }
                crate::term::make_flatmap(heap, &pairs)
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
            // Hash verification is intentionally **not** performed here: the C only
            // checks it in the `decoded_size` sizing pass, against a Bob-Jenkins block
            // hash seeded with the node's `local_node_hash` (`external.c:6437-6442`,
            // `:6471-6512`). That seed is node-identity-owned (`beam_rt`/`beam_dist`),
            // which this codec has none of, so `dec_term`'s skip-the-hash behaviour is
            // faithful.
            Tag::Local => {
                let _node_hash = self.reader.bytes(4)?;
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
    /// A name longer than `MAX_ATOM_CHARS` *characters* is a `SystemLimit`.
    fn atom_term(name: &str, atoms: &mut AtomTable) -> Result<Eterm> {
        if name.chars().count() > MAX_ATOM_CHARS {
            return Err(system_limit());
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
    /// Mirrors `external.c:4464-4497` (`dec_term`): the sign byte is `0` (positive)
    /// or `1` (negative), the magnitude is base-256 **least significant first**, and
    /// a magnitude that fits a single `ErtsDigit` and a 60-bit small decodes to a
    /// small (never a bignum). Leading zero bytes are stripped first, exactly as C
    /// does "to avoid creating illegal bignums".
    fn decode_big(&mut self, tag: Tag, heap: &mut Heap) -> Result<Eterm> {
        let byte_count = if tag == Tag::SmallBig {
            usize::from(self.reader.u8()?)
        } else {
            let count = self.reader.u32be()? as usize;
            if count > BIG_ARITY_MAX * BIG_DIGIT_BYTES {
                return Err(system_limit());
            }
            count
        };
        let negative = match self.reader.u8()? {
            0 => false,
            1 => true,
            // Any other sign byte is malformed.
            _ => return Err(bad_arg()),
        };
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
