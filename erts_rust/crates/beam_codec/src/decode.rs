//! The `Decoder` seam: external term format bytes → terms.
//!
//! Decoding is **allocation-aware**: the decoded term is materialized in the
//! caller-supplied [`beam_alloc::Heap`] (production heap, not the `beam_term`
//! test arena) and every atom is interned into the caller's [`AtomTable`] in wire
//! order. The same code path therefore serves `binary_to_term`, distribution
//! payloads and ETS blobs.
//!
//! This module is safe Rust; the raw heap writes it needs live in
//! [`crate::term`].

use core::str;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    is_atom, make_atom, make_small, try_make_small, MAX_ARITYVAL, MAX_SMALL, MIN_SMALL, NIL,
};
use beam_term::{AtomTable, Eterm};

use crate::atom_cache::AtomCache;
use crate::error::{bad_arg, system_limit, unsupported};
use crate::reader::Reader;
use crate::tag::{Tag, VERSION_MAGIC};

/// The OTP atom-length ceiling, in **characters** (`erts/emulator/beam/erl_term.h`):
/// a longer name is a `SystemLimit`, never a truncated atom.
const MAX_ATOM_CHARS: usize = 255;

/// `BIG_ARITY_MAX` (`erl_term.h:465-469`): the largest bignum digit count.
const BIG_ARITY_MAX: usize = (1 << 16) - 1;

/// `MAP_SMALL_MAP_LIMIT`: the largest flat-map pair count (ERTS switches to a
/// hashmap above it).
const MAP_SMALL_MAP_LIMIT: usize = 32;

/// The largest number of ids an external reference carries (`erl_term.h`,
/// `MAX_REF_IDS`-style bound; `make_external_ref` enforces the same limit).
const MAX_REFERENCE_IDS: usize = 5;

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
    /// Malformed/truncated input, a bad tag, invalid UTF-8 in an atom name or an
    /// unset atom-cache slot yield `BeamError::BadArg`; a value beyond a wire
    /// bound yields `BeamError::SystemLimit`; heap exhaustion propagates from the
    /// heap as `BeamError::Alloc`; a tag the M1 term layer does not support yet
    /// yields [`crate::error::unsupported`].
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
                if len > MAX_REFERENCE_IDS {
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
                if count > MAP_SMALL_MAP_LIMIT {
                    // Larger maps need the HAMT builder (see the crate CONTEXT.md).
                    return Err(unsupported(Tag::Map));
                }
                let mut pairs = Vec::new();
                for _ in 0..count {
                    let key = self.decode(heap, atoms)?;
                    let value = self.decode(heap, atoms)?;
                    pairs.push((key, value));
                }
                crate::term::make_flatmap(heap, &pairs)
            }

            // ---------------------------------------------------------------
            // Deliberately unsupported in M1 — never a wrong encoding
            // ---------------------------------------------------------------
            Tag::Binary
            | Tag::BitBinary
            | Tag::Compressed
            | Tag::NewFun
            | Tag::Export
            | Tag::Fun
            | Tag::V4Port
            | Tag::Local
            | Tag::DistHeader
            | Tag::NewCache
            | Tag::Record => Err(unsupported(tag)),
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
    /// Mirrors `external.c` (`dec_term`): the sign byte is `0` (positive) or `1`
    /// (negative), the digits are base-256 **least significant first**, and a
    /// magnitude that fits a 60-bit small decodes to a small (never a bignum).
    fn decode_big(&mut self, tag: Tag, heap: &mut Heap) -> Result<Eterm> {
        let digit_count = if tag == Tag::SmallBig {
            usize::from(self.reader.u8()?)
        } else {
            let count = self.reader.u32be()? as usize;
            if count > BIG_ARITY_MAX {
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
        let digits = self.reader.bytes(digit_count)?;

        // Strip high-order zero digits (the most significant end of a
        // least-significant-first slice).
        let mut end = digits.len();
        while end > 0 && digits[end - 1] == 0 {
            end -= 1;
        }
        let magnitude = &digits[..end];

        if magnitude.is_empty() {
            return Ok(make_small(0));
        }
        if magnitude.len() <= 8 {
            let mut value: u64 = 0;
            for (i, &digit) in magnitude.iter().enumerate() {
                value |= u64::from(digit) << (8 * i);
            }
            let signed = if negative { -(value as i128) } else { value as i128 };
            if signed >= i128::from(MIN_SMALL) && signed <= i128::from(MAX_SMALL) {
                return Ok(make_small(signed as i64));
            }
        }
        crate::term::make_big(heap, negative, magnitude)
    }
}

/// Decodes a `term_to_binary` image into `heap`, interning atoms into `atoms`.
///
/// Convenience over [`Decoder::new`] + [`Decoder::decode`] + [`Decoder::finish`].
pub fn decode_term(bytes: &[u8], heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
    let mut decoder = Decoder::new(bytes)?;
    let term = decoder.decode(heap, atoms)?;
    decoder.finish()?;
    Ok(term)
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

    use crate::error::is_unsupported;
    use crate::term::{
        big_parts, cons_head, cons_tail, external_pid_parts, external_port_parts,
        external_ref_parts, flatmap_parts, float_value, tuple_arity, tuple_element,
    };

    /// `{ok,17}` (oracle: `term_to_binary({ok,17})`).
    const OK_17: [u8; 9] = [131, 104, 2, 119, 2, 111, 107, 97, 17];
    /// `[1,2,3]` as `STRING_EXT` (oracle: `term_to_binary([1,2,3])`).
    const LIST_123: [u8; 7] = [131, 107, 0, 3, 1, 2, 3];
    /// `#{a=>1}` (oracle: `term_to_binary(#{a=>1})`).
    const MAP_A_1: [u8; 11] = [131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];

    /// Decodes a full image, returning the heap and atom table the term points
    /// into (the term borrows their storage, so all three are returned).
    fn decode(bytes: &[u8]) -> (Heap, AtomTable, Eterm) {
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

    fn assert_bad_arg(bytes: &[u8]) {
        match decode_err(bytes) {
            BeamError::BadArg => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    fn assert_system_limit(bytes: &[u8]) {
        match decode_err(bytes) {
            BeamError::SystemLimit => {}
            other => panic!("expected SystemLimit, got {other:?}"),
        }
    }

    fn assert_unsupported(bytes: &[u8]) {
        let err = decode_err(bytes);
        assert!(is_unsupported(&err), "expected unsupported, got {err:?}");
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
    fn maps_flatmap_and_limit() {
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

        // 33 pairs needs the HAMT builder: reported, never mis-encoded.
        assert_unsupported(&[131, 116, 0, 0, 0, 33]);
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

        // 12345678901234567890123 — too large for a small, decodes to a bignum.
        let (_h, _a, term) = decode(&[
            131, 110, 10, 0, 203, 68, 66, 113, 118, 78, 182, 66, 157, 2,
        ]);
        assert!(!is_small(term));
        let (negative, digits) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(digits, &[203, 68, 66, 113, 118, 78, 182, 66, 157, 2]);

        // `LARGE_BIG_EXT` encodes the same value with a 32-bit digit count.
        let (_h, _a, term) = decode(&[
            131, 111, 0, 0, 0, 10, 0, 203, 68, 66, 113, 118, 78, 182, 66, 157, 2,
        ]);
        let (negative, digits) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(digits, &[203, 68, 66, 113, 118, 78, 182, 66, 157, 2]);

        // The sign byte is 0 or 1 only.
        assert_bad_arg(&[131, 110, 1, 2, 5]);
        // More digits than `BIG_ARITY_MAX` is a hard wire bound.
        assert_system_limit(&[131, 111, 0, 1, 0, 0, 0]);
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

        // `NEW_PORT_EXT`.
        let mut image = vec![131, 89];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&7u32.to_be_bytes());
        image.extend_from_slice(&9u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, number, creation) = external_port_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, creation), (7, 9));

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
        for image in [&OK_17[..], &LIST_123[..], &MAP_A_1[..]] {
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

    #[test]
    fn unknown_and_unsupported_tags() {
        // A byte that is not a tag at all.
        assert_bad_arg(&[131, 200]);
        assert_bad_arg(&[131, 0]);
        // `VERSION_MAGIC` is not a term tag.
        assert_bad_arg(&[131, 131]);

        // Every tag the M1 term layer does not support is reported, not guessed.
        for tag in [
            Tag::Binary,
            Tag::BitBinary,
            Tag::Compressed,
            Tag::NewFun,
            Tag::Export,
            Tag::Fun,
            Tag::V4Port,
            Tag::Local,
            Tag::DistHeader,
            Tag::NewCache,
            Tag::Record,
        ] {
            assert_unsupported(&[131, tag.as_byte()]);
        }

        // The documented oracle vector for `<<"hi">>`.
        assert_unsupported(&[131, 109, 0, 0, 0, 2, 104, 105]);
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
