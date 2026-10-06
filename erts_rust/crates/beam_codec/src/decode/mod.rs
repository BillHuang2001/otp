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
use beam_term::{AtomTable, Eterm};

use crate::atom_cache::AtomCache;
use crate::error::{bad_arg, system_limit};
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
        Ok(Decoder { reader, cache: None })
    }

    /// Creates a decoder for a body: **no** leading version byte.
    pub fn new_body(bytes: &'a [u8]) -> Decoder<'a> {
        Decoder { reader: Reader::new(bytes), cache: None }
    }

    /// Attaches the distribution atom cache used to resolve `ATOM_CACHE_REF` (`82`).
    pub fn with_atom_cache(self, cache: &'a AtomCache) -> Decoder<'a> {
        Decoder { reader: self.reader, cache: Some(cache) }
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
                let creation = u32::from(self.reader.u8()?);
                crate::term::make_external_ref(heap, node, &[id], creation)
            }
            Tag::NewReference => {
                let len = usize::from(self.reader.u16be()?);
                let node = self.decode_node_atom(heap, atoms)?;
                let creation = u32::from(self.reader.u8()?);
                let ids = self.read_ids(len)?;
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
                let node = self.decode_node_atom(heap, atoms)?;
                let number = u64::from(self.reader.u16be()?);
                let creation = u32::from(self.reader.u8()?);
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
                // Every map is stored as a **flatmap**, whatever its pair count: the
                // flatmap/hashmap switchover is an *open interior* layout (workspace
                // `CONTEXT.md` §2) and no HAMT builder exists in the workspace yet.
                // The wire order (interleaved `K1,V1,…,Kn,Vn`) is preserved as-is —
                // deliberately **not** sorted/deduped, unlike ERTS's sorted-flatmap
                // invariant (`erl_map.h:181-199`, `external.c:5042-5100`); a future
                // pass may sort via `beam_term::cmp`.
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

    /// Decodes `BINARY_EXT`/`BIT_BINARY_EXT` into an on-heap bitstring
    /// (`external.c:4896-4923`).
    ///
    /// The wire is `[len:4 BE][data]` for a binary and `[len:4 BE][bits:1][data]`
    /// for a bit binary — the length field comes **before** the trailing-bit count.
    /// `len` is attacker-controlled, so it is checked against the bytes actually
    /// left in the cursor *before* anything is allocated: a lying length is
    /// `BeamError::BadArg`, never a blind allocation. For `BIT_BINARY_EXT` the
    /// trailing-bit count must be `1..=8` and `len * 8` must be at least 8 (i.e.
    /// `len != 0`), exactly as the C, and `size_in_bits = len * 8 - (8 - bits)`.
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
        if tag == Tag::BitBinary {
            if !(1..=8).contains(&trailing_bits) || size_in_bits < 8 {
                return Err(bad_arg());
            }
            let size_in_bits = size_in_bits - (8 - usize::from(trailing_bits));
            crate::term::make_bitstring(heap, data, size_in_bits)
        } else {
            crate::term::make_binary(heap, data)
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
    let body = crate::compress::decompress(&bytes[6..])?;
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
mod tests {
    //! Decode tests: the seeded `term_to_binary` vectors, term structure asserted
    //! through the safe accessors, plus the error mapping (truncation at every
    //! offset, bad tags, version rules, wire bounds and unsupported tags).
    //!
    //! Tests are allowed to `unwrap` (a panic there is a test failure); the
    //! production bodies above never do.

    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_term::prelude::{atom_val, is_list, is_nil, is_small, signed_value};

    use crate::term::{
        big_parts, bitstring_bytes, bitstring_size_bits, cons_head, cons_tail, external_pid_parts,
        external_port_parts, external_ref_parts, flatmap_parts, float_value, tuple_arity,
        tuple_element,
    };

    /// `{ok,17}` (oracle: `term_to_binary({ok,17})`).
    const OK_17: [u8; 9] = [131, 104, 2, 119, 2, 111, 107, 97, 17];
    /// `[1,2,3]` as `STRING_EXT` (oracle: `term_to_binary([1,2,3])`).
    const LIST_123: [u8; 7] = [131, 107, 0, 3, 1, 2, 3];
    /// `#{a=>1}` (oracle: `term_to_binary(#{a=>1})`).
    const MAP_A_1: [u8; 11] = [131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];

    /// Decodes a full image, returning the heap and atom table the term points
    /// into (the term borrows their storage, so all three are returned).
    pub(crate) fn decode(bytes: &[u8]) -> (Heap, AtomTable, Eterm) {
        let mut heap = Heap::new(256).expect("tiny heap allocation");
        let mut atoms = AtomTable::new();
        let term = decode_term(bytes, &mut heap, &mut atoms).expect("valid image decodes");
        (heap, atoms, term)
    }

    /// The error `decode_term` produces for `bytes`.
    fn decode_err(bytes: &[u8]) -> BeamError {
        let mut heap = Heap::new(64).expect("tiny heap allocation");
        let mut atoms = AtomTable::new();
        decode_term(bytes, &mut heap, &mut atoms).expect_err("input must be rejected")
    }

    pub(crate) fn assert_bad_arg(bytes: &[u8]) {
        match decode_err(bytes) {
            BeamError::BadArg => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    pub(crate) fn assert_system_limit(bytes: &[u8]) {
        match decode_err(bytes) {
            BeamError::SystemLimit => {}
            other => panic!("expected SystemLimit, got {other:?}"),
        }
    }

    /// The UTF-8 `"n1"` node atom, as it appears in an external identifier.
    const NODE_N1: [u8; 4] = [119, 2, 110, 49];

    #[test]
    fn integers_small_and_32_bit() {
        let (_h, _a, term) = decode(&[131, 97, 255]);
        assert_eq!(term, make_small(255));

        let (_h, _a, term) = decode(&[131, 98, 0, 0, 0, 7]);
        assert_eq!(term, make_small(7));

        // `INTEGER_EXT` is a signed 32-bit two's-complement value.
        let (_h, _a, term) = decode(&[131, 98, 255, 255, 255, 255]);
        assert_eq!(term, make_small(-1));
        assert_eq!(signed_value(term), -1);

        let (_h, _a, term) = decode(&[131, 98, 128, 0, 0, 0]);
        assert_eq!(signed_value(term), -2_147_483_648);
    }

    #[test]
    fn floats_new_and_legacy() {
        // `NEW_FLOAT_EXT` — the oracle encoding of `3.14`.
        let (_h, _a, term) = decode(&[131, 70, 64, 9, 30, 184, 81, 235, 133, 31]);
        assert_eq!(float_value(term).unwrap(), 3.14);

        // `FLOAT_EXT` — a 31-byte NUL-padded `%.20e` string.
        let mut image = vec![131, 99];
        image.extend_from_slice(b"3.14000000000000000000e+00");
        image.push(0);
        image.resize(2 + 31, 0);
        let (_h, _a, term) = decode(&image);
        assert_eq!(float_value(term).unwrap(), 3.14);

        // Negative zero and NaN survive bit-for-bit.
        let (_h, _a, term) = decode(&[131, 70, 128, 0, 0, 0, 0, 0, 0, 0]);
        assert!(float_value(term).unwrap().is_sign_negative());
        let (_h, _a, term) = decode(&[131, 70, 127, 248, 0, 0, 0, 0, 0, 0]);
        assert!(float_value(term).unwrap().is_nan());

        // `FLOAT_EXT` accepts a short, NUL-terminated rendering.
        let mut image = vec![131, 99];
        image.extend_from_slice(b"1.5");
        image.push(0);
        image.resize(2 + 31, 0);
        let (_h, _a, term) = decode(&image);
        assert_eq!(float_value(term).unwrap(), 1.5);

        let mut bad = vec![131, 99];
        bad.extend_from_slice(b"not a number");
        bad.resize(2 + 31, 0);
        let err = decode_err(&bad);
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    #[test]
    fn all_four_atom_tags_decode() {
        // `ATOM_EXT` (Latin-1).
        let (_h, atoms, term) = decode(&[131, 100, 0, 2, 111, 107]);
        assert_eq!(atoms.name(atom_val(term)), Some("ok"));

        // `SMALL_ATOM_EXT` (Latin-1).
        let (_h, atoms, term) = decode(&[131, 115, 2, 111, 107]);
        assert_eq!(atoms.name(atom_val(term)), Some("ok"));

        // `ATOM_UTF8_EXT`.
        let (_h, atoms, term) = decode(&[131, 118, 0, 2, 111, 107]);
        assert_eq!(atoms.name(atom_val(term)), Some("ok"));

        // `SMALL_ATOM_UTF8_EXT`, multi-byte.
        let (_h, atoms, term) = decode(&[131, 119, 2, 0xC3, 0xA9]);
        assert_eq!(atoms.name(atom_val(term)), Some("é"));

        // Latin-1 high bytes map to `U+0080..=U+00FF`.
        let (_h, atoms, term) = decode(&[131, 115, 1, 0xFF]);
        assert_eq!(atoms.name(atom_val(term)), Some("ÿ"));

        // Invalid UTF-8 in a UTF-8 atom tag is malformed input.
        assert_bad_arg(&[131, 118, 0, 1, 0xFF]);
        assert_bad_arg(&[131, 119, 1, 0xFF]);

        // A zero-length atom name is legal.
        let (_h, atoms, term) = decode(&[131, 115, 0]);
        assert_eq!(atoms.name(atom_val(term)), Some(""));
    }

    #[test]
    fn over_long_atom_names_are_system_limit() {
        // 256 Latin-1 characters (one byte each).
        let mut latin1 = vec![131, 100, 1, 0];
        latin1.extend_from_slice(&[b'a'; 256]);
        assert_system_limit(&latin1);

        // 256 UTF-8 characters.
        let mut utf8 = vec![131, 118, 1, 0];
        utf8.extend_from_slice(&[b'a'; 256]);
        assert_system_limit(&utf8);

        // Exactly 255 characters is at the limit.
        let mut at_limit = vec![131, 100, 0, 255];
        at_limit.extend_from_slice(&[b'a'; 255]);
        let (_h, atoms, term) = decode(&at_limit);
        assert_eq!(atoms.name(atom_val(term)).map(str::len), Some(255));
    }

    #[test]
    fn tuples_nil_and_arity_bounds() {
        // `{ok,17}` — arity-2 tuple, first element the atom `ok`.
        let (_h, atoms, term) = decode(&OK_17);
        assert_eq!(tuple_arity(term).unwrap(), 2);
        let first = tuple_element(term, 0).unwrap();
        assert!(is_atom(first));
        assert_eq!(atoms.name(atom_val(first)), Some("ok"));
        assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));
        assert!(tuple_element(term, 2).is_err());

        // The empty tuple is still a tuple.
        let (_h, _a, term) = decode(&[131, 104, 0]);
        assert_eq!(tuple_arity(term).unwrap(), 0);

        // `LARGE_TUPLE_EXT`.
        let (_h, _a, term) = decode(&[131, 105, 0, 0, 0, 2, 97, 1, 97, 2]);
        assert_eq!(tuple_arity(term).unwrap(), 2);
        assert_eq!(tuple_element(term, 1).unwrap(), make_small(2));

        // `NIL_EXT`.
        let (_h, _a, term) = decode(&[131, 106]);
        assert_eq!(term, NIL);
        assert!(is_nil(term));

        // An arity above `MAX_ARITYVAL` is a hard wire bound.
        assert_system_limit(&[131, 105, 1, 0, 0, 0]);
        // `MAX_ARITYVAL` itself is only rejected for lack of elements.
        assert_bad_arg(&[131, 105, 0, 255, 255, 255]);
    }

    #[test]
    fn strings_and_lists() {
        // `STRING_EXT` → a list of smalls (oracle encoding of `[1,2,3]`).
        let (_h, _a, term) = decode(&LIST_123);
        assert!(is_list(term));
        let first = cons_head(term).unwrap();
        let rest = cons_tail(term).unwrap();
        let second = cons_head(rest).unwrap();
        let rest = cons_tail(rest).unwrap();
        let third = cons_head(rest).unwrap();
        assert_eq!(first, make_small(1));
        assert_eq!(second, make_small(2));
        assert_eq!(third, make_small(3));
        assert!(is_nil(cons_tail(rest).unwrap()));

        // The empty string is `NIL` itself.
        let (_h, _a, term) = decode(&[131, 107, 0, 0]);
        assert_eq!(term, NIL);

        // `LIST_EXT`, proper.
        let (_h, _a, term) = decode(&[131, 108, 0, 0, 0, 2, 97, 1, 97, 2, 106]);
        assert_eq!(cons_head(term).unwrap(), make_small(1));
        let rest = cons_tail(term).unwrap();
        assert_eq!(cons_head(rest).unwrap(), make_small(2));
        assert!(is_nil(cons_tail(rest).unwrap()));

        // `LIST_EXT`, improper: the tail is the following term.
        let (_h, _a, term) = decode(&[131, 108, 0, 0, 0, 1, 97, 7, 97, 9]);
        assert_eq!(cons_head(term).unwrap(), make_small(7));
        assert_eq!(cons_tail(term).unwrap(), make_small(9));

        // An empty `LIST_EXT` is just its tail.
        let (_h, _a, term) = decode(&[131, 108, 0, 0, 0, 0, 97, 5]);
        assert_eq!(term, make_small(5));
        // A non-small list element is fine (list of tuples).
        let (_h, _a, term) = decode(&[131, 108, 0, 0, 0, 1, 104, 0, 106]);
        assert_eq!(tuple_arity(cons_head(term).unwrap()).unwrap(), 0);
    }

    #[test]
    fn maps_flatmap_at_any_pair_count() {
        // `#{a=>1}`.
        let (_h, atoms, term) = decode(&MAP_A_1);
        let (keys, values) = flatmap_parts(term).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(values.len(), 1);
        assert_eq!(atoms.name(atom_val(keys[0])), Some("a"));
        assert_eq!(values[0], make_small(1));

        // An empty map is a flatmap of zero pairs.
        let (_h, _a, term) = decode(&[131, 116, 0, 0, 0, 0]);
        let (keys, values) = flatmap_parts(term).unwrap();
        assert!(keys.is_empty());
        assert!(values.is_empty());

        // 40 pairs (> `MAP_SMALL_MAP_LIMIT`) decodes as a flatmap of 40 pairs, in
        // wire order — no HAMT builder is needed.
        let mut image = vec![131, 116, 0, 0, 0, 40];
        for i in 0..40u8 {
            image.extend_from_slice(&[97, i]); // SMALL_INTEGER_EXT key
            image.extend_from_slice(&[97, i + 100]); // SMALL_INTEGER_EXT value
        }
        let (_h, _a, term) = decode(&image);
        let (keys, values) = flatmap_parts(term).unwrap();
        assert_eq!(keys.len(), 40);
        assert_eq!(values.len(), 40);
        for i in 0..40i64 {
            assert_eq!(keys[i as usize], make_small(i));
            assert_eq!(values[i as usize], make_small(i + 100));
        }

        // A pair count past `MAX_ARITYVAL` cannot be a flatmap `arityval`: a
        // `SystemLimit`, and nothing is read past the count.
        assert_system_limit(&[131, 116, 0x01, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn bignums_decode_to_small_or_bignum() {
        // 300 (`0x012C`) fits a small.
        let (_h, _a, term) = decode(&[131, 110, 2, 0, 44, 1]);
        assert!(is_small(term));
        assert_eq!(signed_value(term), 300);

        // A negative magnitude that fits a small.
        let (_h, _a, term) = decode(&[131, 110, 1, 1, 5]);
        assert_eq!(term, make_small(-5));

        // Zero, with a (stripped) redundant high digit.
        let (_h, _a, term) = decode(&[131, 110, 2, 0, 0, 0]);
        assert_eq!(term, make_small(0));

        // An empty magnitude is zero too.
        let (_h, _a, term) = decode(&[131, 110, 0, 0]);
        assert_eq!(term, make_small(0));

        // 12345678901234567890123 — too large for a small, decodes to a bignum whose
        // ten base-256 bytes become two `ErtsDigit`s.
        let (_h, _a, term) = decode(&[
            131, 110, 10, 0, 203, 68, 66, 113, 118, 78, 182, 66, 157, 2,
        ]);
        assert!(!is_small(term));
        let (negative, digits) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(digits, &[0x42B6_4E76_7142_44CB, 0x029D]);

        // `LARGE_BIG_EXT` encodes the same value with a 32-bit byte count.
        let (_h, _a, term) = decode(&[
            131, 111, 0, 0, 0, 10, 0, 203, 68, 66, 113, 118, 78, 182, 66, 157, 2,
        ]);
        let (negative, digits) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(digits, &[0x42B6_4E76_7142_44CB, 0x029D]);

        // The sign byte is 0 or 1 only.
        assert_bad_arg(&[131, 110, 1, 2, 5]);
        // More base-256 bytes than `BIG_ARITY_MAX * sizeof(ErtsDigit)` (524280) is a
        // hard wire bound (`external.c:6146`).
        assert_system_limit(&[131, 111, 0, 8, 0, 25]);
    }

    #[test]
    fn large_big_accepts_a_byte_count_above_the_digit_count() {
        // The `LARGE_BIG_EXT` bound is a *byte* count (`BIG_ARITY_MAX * sizeof(ErtsDigit)`,
        // i.e. up to 65535 `ErtsDigit`s), so 65536 base-256 bytes = 8192 digits is legal.
        let mut image = vec![131, 111, 0, 1, 0, 0, 0]; // LARGE_BIG_EXT, 65536 bytes, +
        image.extend(std::iter::repeat(0x37).take(65_536));
        let mut heap = Heap::new(8193 + 64).expect("a word heap for the digits");
        let mut atoms = AtomTable::new();
        let term = decode_term(&image, &mut heap, &mut atoms).expect("inside the wire bound");
        let (negative, digits) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(digits.len(), 8192);
        assert!(digits.iter().all(|&digit| digit == 0x3737_3737_3737_3737));
    }

    #[test]
    fn external_identifiers_carry_their_fields() {
        // `NEW_PID_EXT`.
        let mut image = vec![131, 88];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        image.extend_from_slice(&0x0506_0708u32.to_be_bytes());
        image.extend_from_slice(&9u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, number, serial, creation) = external_pid_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, serial, creation), (0x0102_0304, 0x0506_0708, 9));

        // `PID_EXT` (legacy): an 8-bit creation.
        let mut image = vec![131, 103];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&1u32.to_be_bytes());
        image.extend_from_slice(&2u32.to_be_bytes());
        image.push(3);
        let (_h, atoms, term) = decode(&image);
        let (node, number, serial, creation) = external_pid_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, serial, creation), (1, 2, 3));

        // `NEW_PORT_EXT` (decode-only; the encoder never emits it).
        let mut image = vec![131, 89];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&7u32.to_be_bytes());
        image.extend_from_slice(&9u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, number, creation) = external_port_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, creation), (7, 9));

        // `V4_PORT_EXT`: an 8-byte id — the form the C encoder always emits.
        let mut image = vec![131, 120];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&0x0000_0007_0000_0001u64.to_be_bytes());
        image.extend_from_slice(&9u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, number, creation) = external_port_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, creation), (0x0000_0007_0000_0001, 9));

        // `V4_PORT_EXT` with the maximum 64-bit id.
        let mut image = vec![131, 120];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&u64::MAX.to_be_bytes());
        image.extend_from_slice(&u32::MAX.to_be_bytes());
        let (_h, _a, term) = decode(&image);
        let (number, creation) = (external_port_parts(term).unwrap().1, external_port_parts(term).unwrap().2);
        assert_eq!((number, creation), (u64::MAX, u32::MAX));

        // A `V4_PORT_EXT` whose id field is truncated is malformed.
        assert_bad_arg(&[131, 120, 119, 2, 110, 49, 0, 0, 0, 0]);

        // `PORT_EXT` (legacy): a 16-bit id, 8-bit creation.
        let mut image = vec![131, 102];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&7u16.to_be_bytes());
        image.push(9);
        let (_h, _a, term) = decode(&image);
        assert_eq!(external_port_parts(term).unwrap().1, 7);
        assert_eq!(external_port_parts(term).unwrap().2, 9);

        // `NEWER_REFERENCE_EXT`.
        let mut image = vec![131, 90, 0, 2];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&9u32.to_be_bytes());
        image.extend_from_slice(&1u32.to_be_bytes());
        image.extend_from_slice(&2u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, ids, creation) = external_ref_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!(ids, &[1, 2]);
        assert_eq!(creation, 9);

        // `NEW_REFERENCE_EXT`: 8-bit creation.
        let mut image = vec![131, 114, 0, 1];
        image.extend_from_slice(&NODE_N1);
        image.push(7);
        image.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        let (_h, _a, term) = decode(&image);
        let (_, ids, creation) = external_ref_parts(term).unwrap();
        assert_eq!(ids, &[0xDEAD_BEEF]);
        assert_eq!(creation, 7);

        // `REFERENCE_EXT` (legacy).
        let mut image = vec![131, 101];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        image.push(5);
        let (_h, _a, term) = decode(&image);
        let (_, ids, creation) = external_ref_parts(term).unwrap();
        assert_eq!(ids, &[0xDEAD_BEEF]);
        assert_eq!(creation, 5);

        // A node field that is not an atom is malformed.
        assert_bad_arg(&[131, 88, 97, 5, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]);
        assert_bad_arg(&[131, 101, 106, 0, 0, 0, 1, 5]);

        // `NEWER_REFERENCE_EXT` carries at most five ids.
        assert_system_limit(&[131, 90, 0, 6]);
    }

    #[test]
    fn atom_cache_refs_resolve_or_fail() {
        // Without a cache an `ATOM_CACHE_REF` cannot be resolved.
        assert_bad_arg(&[131, 82, 0]);

        // An unset slot is malformed input.
        let cache = AtomCache::new();
        let bytes = [131u8, 82, 3];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new(&bytes).unwrap().with_atom_cache(&cache);
        let err = decoder.decode(&mut heap, &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // A set slot yields the atom it recorded.
        let mut cache = AtomCache::new();
        let mut atoms = AtomTable::new();
        let cached = atoms.intern("cached");
        cache.set(3, cached);
        let mut heap = Heap::new(64).unwrap();
        let mut decoder = Decoder::new(&bytes).unwrap().with_atom_cache(&cache);
        let term = decoder.decode(&mut heap, &mut atoms).unwrap();
        decoder.finish().unwrap();
        assert_eq!(term, make_atom(cached));
    }

    #[test]
    fn truncation_at_every_offset_is_bad_arg() {
        // `<<"hi">>` and `<<1:3>>` exercise the binary / bit-binary length+payload
        // headers as well.
        for image in [
            &OK_17[..],
            &LIST_123[..],
            &MAP_A_1[..],
            &[131, 109, 0, 0, 0, 2, b'h', b'i'],
            &[131, 77, 0, 0, 0, 1, 3, 0b0010_0000],
        ] {
            for len in 0..image.len() {
                let mut heap = Heap::new(256).unwrap();
                let mut atoms = AtomTable::new();
                let result = decode_term(&image[..len], &mut heap, &mut atoms);
                assert!(
                    matches!(result, Err(BeamError::BadArg)),
                    "offset {len} of {image:?} must be rejected, got {result:?}"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Binaries and bitstrings
    // -----------------------------------------------------------------------

    /// The oracle images, captured from OTP 29.1.1 with
    /// `io:format("~w~n", [term_to_binary(T)])`.
    #[test]
    fn binary_and_bit_binary_decode() {
        // `<<"hi">>`: `[109][len:4 BE][data]`.
        let (_h, _a, term) = decode(&[131, 109, 0, 0, 0, 2, 104, 105]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 16);
        assert_eq!(bitstring_bytes(term).unwrap(), (&[104u8, 105][..], 0, 16));

        // `<<>>`: a zero-length binary is legal (only `BIT_BINARY_EXT` rejects 0).
        let (_h, _a, term) = decode(&[131, 109, 0, 0, 0, 0]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 0);
        assert_eq!(bitstring_bytes(term).unwrap(), (&[][..], 0, 0));

        // `<<1:3>>`: `[77][len:4 BE][bits:1][data]` — the length precedes the
        // trailing-bit count, and the byte is `0b0010_0000` (MSB-first).
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 1, 3, 0b0010_0000]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 3);
        assert_eq!(
            bitstring_bytes(term).unwrap(),
            (&[0b0010_0000u8][..], 0, 3)
        );

        // `<<5:12>>`: 2 bytes, 4 significant bits in the last one.
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 2, 4, 0x00, 0x50]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 12);
        assert_eq!(bitstring_bytes(term).unwrap(), (&[0x00u8, 0x50][..], 0, 12));

        // `<<3:13>>`.
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 2, 5, 0x00, 0x18]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 13);

        // `<<1:1>>`: `len = 1`, `bits = 1` ⇒ 1 bit (the C allows `size_in_bits < 8`
        // after the trailing-bit adjustment, as long as `len != 0`).
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 1, 1, 0b1000_0000]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 1);

        // A trailing-bit count of 8 is accepted for backwards compatibility and is
        // equivalent to the byte-aligned form.
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 2, 8, b'h', b'i']);
        assert_eq!(bitstring_size_bits(term).unwrap(), 16);

        // The trailing (non-significant) bits of the last byte are normalised to zero.
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 1, 3, 0b0011_1111]);
        assert_eq!(
            bitstring_bytes(term).unwrap(),
            (&[0b0010_0000u8][..], 0, 3)
        );
    }

    #[test]
    fn malformed_binaries_and_bit_strings_are_bad_arg() {
        // A trailing-bit count outside `1..=8`.
        assert_bad_arg(&[131, 77, 0, 0, 0, 1, 0, 0x00]);
        assert_bad_arg(&[131, 77, 0, 0, 0, 1, 9, 0x00]);
        assert_bad_arg(&[131, 77, 0, 0, 0, 1, 255, 0x00]);
        // Zero bytes: `len * 8 < 8`.
        assert_bad_arg(&[131, 77, 0, 0, 0, 0, 8]);
        // A length that lies past the end of the buffer (payload missing) — rejected
        // *before* anything is allocated.
        assert_bad_arg(&[131, 109, 0, 0, 0, 2, b'h']);
        assert_bad_arg(&[131, 109, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_bad_arg(&[131, 77, 0, 0, 0, 2, 3, b'h']);
        // A bit-binary without even its trailing-bit byte.
        assert_bad_arg(&[131, 77, 0, 0, 0, 1]);
    }

    #[test]
    fn unknown_and_unsupported_tags() {
        // A byte that is not a tag at all.
        assert_bad_arg(&[131, 200]);
        assert_bad_arg(&[131, 0]);
        // `VERSION_MAGIC` is not a term tag.
        assert_bad_arg(&[131, 131]);

        // Obsolete/dead tags are *malformed* (`BadArg`), exactly as the C rejects
        // them: `FUN_EXT` (removed in OTP 23) and `NEW_CACHE` (dead, `#if 0`).
        assert_bad_arg(&[131, Tag::Fun.as_byte()]);
        assert_bad_arg(&[131, Tag::NewCache.as_byte()]);

        // `RECORD_EXT` (67) is implemented now, so a bare tag byte is a *truncation*
        // (the count, flags and fields are missing) — BadArg, never `unsupported`.
        assert_bad_arg(&[131, Tag::Record.as_byte()]);

        // `LOCAL_EXT` (121) is honoured now as the `[local]` wrapper, so a bare tag
        // byte is a *truncation* (the 4-byte hash and the term are missing) — BadArg,
        // never `unsupported`.
        assert_bad_arg(&[131, Tag::Local.as_byte()]);

        // `DIST_HEADER` (68) is a distribution-transport wrapper, never a term: the C
        // term decoder rejects it as malformed, so it is BadArg, never `unsupported`.
        assert_bad_arg(&[131, Tag::DistHeader.as_byte()]);

        // `NEW_FUN_EXT`/`EXPORT_EXT` are implemented now, so a bare tag byte is a
        // *truncation* (the C would also run off the end and error), never
        // `unsupported`.
        assert_bad_arg(&[131, Tag::NewFun.as_byte()]);
        assert_bad_arg(&[131, Tag::Export.as_byte()]);

        // `COMPRESSED` is *covered* now, but only as the top-level wrapper: as a bare
        // tag (a 2-byte image, below the 6-byte minimum) it is malformed, and nested
        // inside a term it is malformed too — never `unsupported`, never a term.
        assert_bad_arg(&[131, 80]);
        assert_bad_arg(&[131, 104, 1, 80]);
    }

    // -----------------------------------------------------------------------
    // `LOCAL_EXT` (121) — the `term_to_binary/2 [local]` wrapper (not a term tag)
    // -----------------------------------------------------------------------

    #[test]
    fn local_ext_wrapper_consumes_hash_and_decodes_the_next_term() {
        // `term_to_binary({ok,17}, [local])`:
        // `[VERSION_MAGIC][LOCAL_EXT][node-hash:4 BE][<term>]`. The node hash is
        // opaque to the decoder (verification needs the node's `local_node_hash`), so
        // any 4 bytes must be accepted and skipped.
        let mut image = vec![131u8, 121, 0xDE, 0xAD, 0xBE, 0xEF];
        image.extend_from_slice(&[104, 2, 119, 2, 111, 107, 97, 17]); // {ok,17}

        let (_h, atoms, term) = decode(&image);
        assert_eq!(tuple_arity(term).unwrap(), 2);
        assert_eq!(
            atoms.name(atom_val(tuple_element(term, 0).unwrap())),
            Some("ok")
        );
        assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));

        // The wrapper is honoured inline too: nested as a sub-term of a tuple, the
        // 4-byte hash is consumed and the following object is the element (the C does
        // the same with `goto continue_this_obj`).
        let nested = [131u8, 104, 1, 121, 0, 0, 0, 0, 97, 7];
        let (_h, _a, term) = decode(&nested);
        assert_eq!(tuple_arity(term).unwrap(), 1);
        assert_eq!(tuple_element(term, 0).unwrap(), make_small(7));
    }

    #[test]
    fn local_ext_body_decoder_path() {
        // A body (no version byte) that starts with the `[local]` wrapper.
        let body = [121u8, 0xAA, 0xBB, 0xCC, 0xDD, 97, 5];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new_body(&body);
        let term = decoder.decode(&mut heap, &mut atoms).unwrap();
        assert_eq!(term, make_small(5));
        assert_eq!(decoder.remaining(), 0);
        decoder.finish().unwrap();
    }

    #[test]
    fn local_ext_truncation_is_bad_arg() {
        // The complete image `[VERSION_MAGIC][LOCAL_EXT][hash:4][{ok,17}]`.
        let full = [131u8, 121, 0, 0, 0, 1, 104, 2, 119, 2, 111, 107, 97, 17];

        // Truncation at *every* prefix is a typed BadArg, never a panic.
        for len in 0..full.len() {
            assert_bad_arg(&full[..len]);
        }

        // The specific boundaries the wrapper introduces: no hash, a partial hash and
        // a full hash with no term following.
        assert_bad_arg(&[131, 121]);
        assert_bad_arg(&[131, 121, 0, 0, 0]);
        assert_bad_arg(&[131, 121, 0, 0, 0, 0]);

        // A body decoder sees the same truncations (no version byte to absorb).
        for len in 0..(full.len() - 1) {
            let body = &full[1..][..len];
            let mut heap = Heap::new(64).unwrap();
            let mut atoms = AtomTable::new();
            let mut decoder = Decoder::new_body(body);
            let err = decoder.decode(&mut heap, &mut atoms).unwrap_err();
            assert!(matches!(err, BeamError::BadArg), "got {err:?} for {body:?}");
        }
    }
    // -----------------------------------------------------------------------
    // `COMPRESSED` (`80`) — the top-level zlib wrapper
    // -----------------------------------------------------------------------

    /// `term_to_binary(lists:duplicate(500, hello), [compressed])` — an oracle image
    /// captured from OTP 29.1.1. Layout: `131, 80, <inner size:4 BE>, <zlib stream>`;
    /// the declared inner size is `3506`, the length of `term_to_binary(List)` minus
    /// its version byte.
    const COMPRESSED_DUP_500_HELLO: [u8; 50] = [
        131, 80, 0, 0, 13, 178, 120, 156, 237, 197, 177, 9, 0, 32, 16, 4, 176, 183, 112, 84, 65,
        228, 192, 210, 197, 29, 192, 53, 44, 146, 38, 169, 106, 247, 244, 57, 146, 45, 73, 146,
        36, 73, 250, 168, 245, 0, 145, 146, 3, 87,
    ];

    #[test]
    fn compressed_top_level_wrapper_is_unwrapped() {
        // The inner body needs 500 cons cells: a heap of 256 words is too small.
        let mut heap = Heap::new(4096).unwrap();
        let mut atoms = AtomTable::new();
        let term = decode_term(&COMPRESSED_DUP_500_HELLO, &mut heap, &mut atoms).unwrap();

        // The inner term is `[hello, hello, …]` — 500 atoms, proper tail.
        let mut count = 0usize;
        let mut cursor = term;
        while is_list(cursor) {
            let head = cons_head(cursor).unwrap();
            assert!(is_atom(head), "expected an atom element, got {head:?}");
            assert_eq!(atoms.name(atom_val(head)), Some("hello"));
            cursor = cons_tail(cursor).unwrap();
            count += 1;
        }
        assert!(is_nil(cursor), "the inner list must be proper");
        assert_eq!(count, 500);
    }

    #[test]
    fn hand_built_compressed_image_decodes() {
        // The inner body of `{ok,17}` — the `OK_17` image without its version byte.
        let body: &[u8] = &[104, 2, 119, 2, 111, 107, 97, 17];
        let zlib = crate::compress::compress(body, 6).unwrap();
        let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
        image.extend_from_slice(&(body.len() as u32).to_be_bytes());
        image.extend_from_slice(&zlib);

        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let term = decode_term(&image, &mut heap, &mut atoms).unwrap();
        assert_eq!(tuple_arity(term).unwrap(), 2);
        assert_eq!(
            atoms.name(atom_val(tuple_element(term, 0).unwrap())),
            Some("ok")
        );
        assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));
    }

    #[test]
    fn compressed_negative_cases_are_bad_arg() {
        let image = COMPRESSED_DUP_500_HELLO;

        // Fewer than 6 bytes: the C does not even take the `COMPRESSED` branch
        // (it needs the tag plus the 4-byte size, `external.c:1759`), so the `80` is
        // treated as an ordinary — and therefore invalid — term tag.
        assert_bad_arg(&[131, 80]);
        assert_bad_arg(&[131, 80, 0, 0, 13]);

        // A declared size that disagrees with the actual decompressed length.
        let mut wrong_size = image.to_vec();
        wrong_size[2..6].copy_from_slice(&1u32.to_be_bytes());
        assert_bad_arg(&wrong_size);

        // A corrupt Adler-32 trailer: zlib rejects the stream.
        let mut corrupt = image.to_vec();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        assert_bad_arg(&corrupt);

        // A payload that is not a zlib stream at all.
        assert_bad_arg(&[131, 80, 0, 0, 0, 10, 1, 2, 3, 4, 5, 6]);

        // Every truncation past the 6-byte header must be rejected, never a panic.
        for len in 6..image.len() {
            assert_bad_arg(&image[..len]);
        }
    }
    #[test]
    fn version_byte_is_required_and_checked() {
        // Missing (empty input).
        assert_bad_arg(&[]);
        // Missing (a bare body).
        assert_bad_arg(&[97, 17]);
        // Wrong version.
        assert_bad_arg(&[130, 97, 17]);

        assert!(matches!(Decoder::new(&[]).err(), Some(BeamError::BadArg)));
        assert!(matches!(
            Decoder::new(&[130]).err(),
            Some(BeamError::BadArg)
        ));
        assert!(Decoder::new(&[131]).is_ok());
    }

    #[test]
    fn cursor_position_remaining_and_finish() {
        let bytes = [131u8, 97, 17, 0];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new(&bytes).unwrap();
        // The version byte is consumed by `new`.
        assert_eq!(decoder.position(), 1);
        assert_eq!(decoder.remaining(), 3);

        let term = decoder.decode(&mut heap, &mut atoms).unwrap();
        assert_eq!(term, make_small(17));
        assert_eq!(decoder.position(), 3);
        assert_eq!(decoder.remaining(), 1);

        // The trailing byte is rejected by `finish`.
        let err = decoder.finish().unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // `decode_term` reports the same trailing-byte error.
        assert_bad_arg(&bytes);
    }

    #[test]
    fn new_body_has_no_version_byte() {
        let body = [97u8, 17];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new_body(&body);
        assert_eq!(decoder.position(), 0);
        assert_eq!(decoder.decode(&mut heap, &mut atoms).unwrap(), make_small(17));
        assert_eq!(decoder.remaining(), 0);
        decoder.finish().unwrap();

        // A versioned image fed to `new_body` sees `131`, which is not a tag.
        let versioned = [131u8, 97, 17];
        let mut decoder = Decoder::new_body(&versioned);
        let err = decoder.decode(&mut heap, &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }
}
