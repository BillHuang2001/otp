//! The `Encoder` seam: terms → external term format bytes.
//!
//! One encoder serves both `term_to_binary` bytes (with the leading
//! [`VERSION_MAGIC`](crate::VERSION_MAGIC)) and distribution payload/ETS bodies
//! (without it) — the constructor decides which. Term inspection goes through
//! [`crate::term`]; this module owns only the *bytes*.
//!
//! This module is safe Rust.

use beam_common::Result;
use beam_term::prelude::{atom_val, is_atom, is_list, is_nil, is_small, signed_value};
use beam_term::{AtomTable, Eterm};

use crate::error;
use crate::tag::{
    ATOM_EXT, ATOM_UTF8_EXT, INTEGER_EXT, LARGE_BIG_EXT, LARGE_TUPLE_EXT, LIST_EXT, MAP_EXT,
    NEWER_REFERENCE_EXT, NEW_FLOAT_EXT, NEW_PID_EXT, NEW_PORT_EXT, NIL_EXT, SMALL_ATOM_EXT,
    SMALL_ATOM_UTF8_EXT, SMALL_BIG_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, STRING_EXT, Tag,
};
use crate::term::{
    big_parts, cons_head, cons_tail, external_pid_parts, external_port_parts, external_ref_parts,
    flatmap_parts, float_value, tuple_arity, tuple_element,
};
use crate::writer::Writer;

/// `MAP_SMALL_MAP_LIMIT`: the largest flat-map pair count the encoder emits (ERTS
/// switches to a hashmap above it, which the M1 term layer cannot build — see the
/// crate `CONTEXT.md`, "Known Issues").
const MAP_SMALL_MAP_LIMIT: usize = 32;

/// `SMALL_BIG_EXT` carries an 8-bit digit count; larger magnitudes use
/// `LARGE_BIG_EXT`.
const SMALL_BIG_DIGIT_MAX: usize = 255;

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
        }
    }

    /// Selects the atom tag family; defaults to [`AtomEncoding::Utf8`].
    pub fn set_atom_encoding(&mut self, encoding: AtomEncoding) {
        self.atom_encoding = encoding;
    }

    /// Encodes `term`, appending it to the output.
    ///
    /// A tag the M1 term layer does not support yields
    /// [`crate::error::unsupported`]; a value beyond a wire bound yields
    /// `BeamError::SystemLimit`. The encoder never silently mis-encodes.
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
        if let Ok((negative, digits)) = big_parts(term) {
            return self.encode_bignum(negative, digits);
        }
        if let Ok(value) = float_value(term) {
            self.buf.u8(NEW_FLOAT_EXT);
            self.buf.u64be(value.to_bits());
            return Ok(());
        }
        if let Ok(arity) = tuple_arity(term) {
            return self.encode_tuple(term, arity);
        }
        if let Ok((keys, values)) = flatmap_parts(term) {
            return self.encode_map(keys, values);
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
        // Anything else (in practice a binary, which the Stage-0 foundations cannot
        // build) is not representable by the M1 term layer: report it explicitly
        // rather than emit a wrong encoding. Tests never reach this path.
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
        // Magnitude as base-256 little-endian digits; the sign is carried separately.
        self.encode_bignum(value < 0, &value.unsigned_abs().to_le_bytes())
    }

    /// Encodes a bignum from its `(negative, digits)` parts: `digits` are base-256
    /// **little-endian** magnitude bytes. High-order zero bytes are stripped; an
    /// empty magnitude is the small integer `0` on the wire.
    fn encode_bignum(&mut self, negative: bool, digits: &[u8]) -> Result<()> {
        let digits = strip_high_zero_bytes(digits);
        if digits.is_empty() {
            // A zero magnitude coincides with the `SMALL_INTEGER_EXT` encoding of 0,
            // so decoding the output round-trips to a small integer, as in C.
            self.buf.u8(SMALL_INTEGER_EXT);
            self.buf.u8(0);
            return Ok(());
        }
        let sign = u8::from(negative);
        if digits.len() <= SMALL_BIG_DIGIT_MAX {
            self.buf.u8(SMALL_BIG_EXT);
            self.buf.u8(digits.len() as u8);
        } else {
            // The digit count is bounded by `BIG_ARITY_MAX` (65535), so it always
            // fits the 32-bit `LARGE_BIG_EXT` length field.
            self.buf.u8(LARGE_BIG_EXT);
            self.buf.u32be(digits.len() as u32);
        }
        self.buf.u8(sign);
        self.buf.bytes(digits);
        Ok(())
    }

    /// Encodes an atom in the selected [`AtomEncoding`].
    fn encode_atom(&mut self, term: Eterm) -> Result<()> {
        let name = self.atoms.name(atom_val(term)).ok_or_else(error::bad_arg)?;
        // OTP caps atom names at 255 *characters*, independent of the encoding.
        if name.chars().count() > ATOM_CHAR_MAX {
            return Err(error::system_limit());
        }
        match self.atom_encoding {
            AtomEncoding::Utf8 => {
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
            AtomEncoding::Latin1 => {
                // Latin-1 is one byte per code point and only covers `U+0000..=U+00FF`;
                // a wider character cannot be represented, so it is reported as
                // unsupported rather than mis-encoded.
                let mut bytes = Vec::with_capacity(name.len());
                for ch in name.chars() {
                    let code = ch as u32;
                    if code > 0xFF {
                        return Err(error::unsupported(Tag::SmallAtom));
                    }
                    bytes.push(code as u8);
                }
                if bytes.len() <= 255 {
                    self.buf.u8(SMALL_ATOM_EXT);
                    self.buf.u8(bytes.len() as u8);
                } else {
                    self.buf.u8(ATOM_EXT);
                    self.buf.u16be(bytes.len() as u16);
                }
                self.buf.bytes(&bytes);
            }
        }
        Ok(())
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

    /// Encodes a flat map (≤ `MAP_SMALL_MAP_LIMIT` pairs) as `MAP_EXT`.
    ///
    /// A larger map needs the HAMT builder the M1 term layer does not have, so it
    /// is reported as unsupported.
    fn encode_map(&mut self, keys: &[Eterm], values: &[Eterm]) -> Result<()> {
        let n = keys.len();
        if n > MAP_SMALL_MAP_LIMIT {
            return Err(error::unsupported(Tag::Map));
        }
        debug_assert_eq!(n, values.len(), "a flatmap's keys and values are paired");
        self.buf.u8(MAP_EXT);
        self.buf.u32be(n as u32);
        for i in 0..n {
            self.encode(keys[i])?;
            self.encode(values[i])?;
        }
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

    /// Encodes an external port as `NEW_PORT_EXT` (the modern form). A 64-bit port
    /// id cannot be represented by that tag's 32-bit field; `V4_PORT_EXT` is out of
    /// M1 scope, so a wider id is a `SystemLimit` rather than a truncation.
    fn encode_port(&mut self, node: Eterm, number: u64, creation: u32) -> Result<()> {
        if number > u32::MAX as u64 {
            return Err(error::system_limit());
        }
        self.buf.u8(NEW_PORT_EXT);
        self.encode(node)?;
        self.buf.u32be(number as u32);
        self.buf.u32be(creation);
        Ok(())
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
}

/// Strips the high-order zero bytes of a base-256 little-endian magnitude.
fn strip_high_zero_bytes(digits: &[u8]) -> &[u8] {
    let mut end = digits.len();
    while end > 0 && digits[end - 1] == 0 {
        end -= 1;
    }
    &digits[..end]
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

#[cfg(test)]
mod tests {
    //! Byte-exact encoder tests: every tag family is asserted against the exact
    //! wire bytes, including the width-policy boundaries. Terms are built with
    //! `beam_alloc::Heap` + the [`crate::term`] constructors.

    use beam_alloc::Heap;
    use beam_common::{BeamError, Result};
    use beam_term::prelude::{make_atom, make_small, Atom, NIL};

    use super::*;
    use crate::error::is_unsupported;
    use crate::term::{
        make_big, make_external_pid, make_external_port, make_external_ref, make_flatmap,
        make_float,
    };

    /// A heap large enough for every term a test builds (tests pass a bigger one
    /// when they need it).
    fn new_heap(words: usize) -> Heap {
        Heap::new(words).expect("the carrier allocator never fails for a test heap")
    }

    fn assert_bad_arg<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    fn assert_system_limit<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::SystemLimit) => {}
            other => panic!("expected SystemLimit, got {other:?}"),
        }
    }

    /// Builds a proper list from `elems` (head first) on `heap`.
    fn build_list(heap: &mut Heap, elems: &[i64]) -> Eterm {
        let mut list = NIL;
        for &value in elems.iter().rev() {
            list = heap.cons(make_small(value), list).expect("list fits");
        }
        list
    }

    #[test]
    fn small_integers_follow_the_width_policy() {
        let atoms = AtomTable::new();
        let cases: &[(i64, &[u8])] = &[
            (0, &[97, 0]),
            (1, &[97, 1]),
            (255, &[97, 255]),
            (256, &[98, 0, 0, 1, 0]),
            (-1, &[98, 255, 255, 255, 255]),
            (i64::from(i32::MAX), &[98, 127, 255, 255, 255]),
            (i64::from(i32::MIN), &[98, 128, 0, 0, 0]),
            // Just past the signed 32-bit range ⇒ bignum, magnitude little-endian.
            (i64::from(i32::MAX) + 1, &[110, 4, 0, 0, 0, 0, 128]),
            (i64::from(i32::MIN) - 1, &[110, 4, 1, 1, 0, 0, 128]),
            (1i64 << 40, &[110, 6, 0, 0, 0, 0, 0, 0, 1]),
        ];
        for (value, expected) in cases {
            let bytes = encode_term(make_small(*value), &atoms).unwrap();
            assert_eq!(bytes[0], 131);
            assert_eq!(&bytes[1..], *expected, "small {value} mis-encoded");
        }
    }

    #[test]
    fn bignum_terms_use_small_big_and_strip_high_zeros() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);

        let t = make_big(&mut heap, false, &[0x05]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![110, 1, 0, 5]);

        let t = make_big(&mut heap, true, &[0x0A]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![110, 1, 1, 10]);

        // Digits are little-endian, so high-order zero bytes sit at the end.
        let t = make_big(&mut heap, false, &[5, 0, 0]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![110, 1, 0, 5]);

        // An all-zero magnitude coincides with the small integer 0 on the wire.
        let t = make_big(&mut heap, false, &[0, 0]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![97, 0]);
    }

    #[test]
    fn bignum_with_at_least_256_digits_uses_large_big() {
        let atoms = AtomTable::new();
        let digits = vec![1u8; 256];
        let mut heap = new_heap(4096);
        let t = make_big(&mut heap, false, &digits).unwrap();

        let bytes = encode_term_body(t, &atoms).unwrap();
        assert_eq!(bytes[0], 111);
        assert_eq!(&bytes[1..5], &[0, 0, 1, 0]); // 256, big-endian u32
        assert_eq!(bytes[5], 0); // sign
        assert_eq!(&bytes[6..], &digits[..]);
        assert_eq!(bytes.len(), 1 + 4 + 1 + 256);
    }

    #[test]
    fn atoms_use_small_utf8_by_default() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("ok");
        assert_eq!(
            encode_term(make_atom(a), &atoms).unwrap(),
            vec![131, 119, 2, 111, 107]
        );
    }

    #[test]
    fn atoms_can_be_encoded_as_latin1() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("ok");
        let mut encoder = Encoder::new(&atoms);
        encoder.set_atom_encoding(AtomEncoding::Latin1);
        encoder.encode(make_atom(a)).unwrap();
        assert_eq!(encoder.finish(), vec![131, 115, 2, 111, 107]);
    }

    #[test]
    fn long_and_multibyte_atom_names_pick_the_wide_tags() {
        let mut atoms = AtomTable::new();

        // 255 one-byte characters: the largest `SMALL_ATOM_UTF8_EXT`.
        let a = atoms.intern(&"a".repeat(255));
        let bytes = encode_term_body(make_atom(a), &atoms).unwrap();
        assert_eq!(&bytes[..2], &[119, 255]);
        assert_eq!(bytes.len(), 2 + 255);

        // 200 two-byte characters (400 UTF-8 bytes) force `ATOM_UTF8_EXT`, but are
        // only 200 characters, so they stay within the OTP limit.
        let e_acute = atoms.intern(&"\u{00e9}".repeat(200));
        let bytes = encode_term_body(make_atom(e_acute), &atoms).unwrap();
        assert_eq!(&bytes[..3], &[118, 0x01, 0x90]); // 400 = 0x0190
        assert_eq!(bytes.len(), 3 + 400);

        // The same name in Latin-1 is 200 one-byte code points ⇒ `SMALL_ATOM_EXT`.
        let mut encoder = Encoder::new_body(&atoms);
        encoder.set_atom_encoding(AtomEncoding::Latin1);
        encoder.encode(make_atom(e_acute)).unwrap();
        let latin = encoder.finish();
        assert_eq!(&latin[..2], &[115, 200]);
        assert_eq!(latin.len(), 2 + 200);
        assert_eq!(latin[2], 0xE9);
    }

    #[test]
    fn over_long_atom_names_are_a_system_limit() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern(&"a".repeat(256));
        assert_system_limit(encode_term(make_atom(a), &atoms));
    }

    #[test]
    fn non_latin1_atom_under_latin1_is_unsupported() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("\u{03bb}"); // Greek small letter lambda, U+03BB
        let mut encoder = Encoder::new(&atoms);
        encoder.set_atom_encoding(AtomEncoding::Latin1);
        let err = encoder.encode(make_atom(a)).unwrap_err();
        assert!(is_unsupported(&err));

        // The same atom is fine as UTF-8 (λ is two UTF-8 bytes).
        assert_eq!(
            encode_term_body(make_atom(a), &atoms).unwrap(),
            vec![119, 2, 0xCE, 0xBB]
        );
    }

    #[test]
    fn unknown_atom_index_is_bad_arg() {
        let atoms = AtomTable::new();
        // An atom index the table never interned has no name to render.
        let ghost = make_atom(Atom::from_raw(0xDEAD_BEEF));
        assert_bad_arg(encode_term(ghost, &atoms));
    }

    #[test]
    fn nil_encodes_to_106() {
        let atoms = AtomTable::new();
        assert_eq!(encode_term(NIL, &atoms).unwrap(), vec![131, 106]);
        assert_eq!(encode_term_body(NIL, &atoms).unwrap(), vec![106]);
    }

    #[test]
    fn proper_byte_lists_use_string_ext() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
        let list = build_list(&mut heap, &[1, 2, 3]);
        assert_eq!(encode_term_body(list, &atoms).unwrap(), vec![107, 0, 3, 1, 2, 3]);
    }

    #[test]
    fn a_list_with_a_non_byte_element_uses_list_ext() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
        // 256 is a small int but not a byte, so the whole list is `LIST_EXT`.
        let list = build_list(&mut heap, &[1, 256]);
        assert_eq!(
            encode_term_body(list, &atoms).unwrap(),
            vec![108, 0, 0, 0, 2, 97, 1, 98, 0, 0, 1, 0, 106]
        );
    }

    #[test]
    fn improper_lists_use_list_ext_and_encode_the_tail() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
        let list = heap.cons(make_small(1), make_small(2)).unwrap();
        assert_eq!(
            encode_term_body(list, &atoms).unwrap(),
            vec![108, 0, 0, 0, 1, 97, 1, 97, 2]
        );
    }

    #[test]
    fn byte_list_of_65535_uses_string_ext() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(200_000);
        let mut list = NIL;
        for _ in 0..65_535 {
            list = heap.cons(make_small(0), list).unwrap();
        }
        let bytes = encode_term_body(list, &atoms).unwrap();
        assert_eq!(bytes[0], 107);
        assert_eq!(&bytes[1..3], &[0xFF, 0xFF]);
        assert_eq!(bytes.len(), 3 + 65_535);
    }

    #[test]
    fn byte_list_of_65536_uses_list_ext_not_an_error() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(200_000);
        let mut list = NIL;
        for _ in 0..65_536 {
            list = heap.cons(make_small(0), list).unwrap();
        }
        let bytes = encode_term_body(list, &atoms).unwrap();
        assert_eq!(bytes[0], 108);
        assert_eq!(&bytes[1..5], &[0, 1, 0, 0]); // 65536, big-endian u32
        // Each element is the small int 0 → [97, 0]; the tail is NIL (106).
        assert_eq!(bytes.len(), 5 + 65_536 * 2 + 1);
        assert_eq!(*bytes.last().unwrap(), 106);
    }

    #[test]
    fn tuple_arity_255_and_256_pick_the_right_tag() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(1024);

        let t = heap.tuple(&vec![make_small(0); 255]).unwrap();
        let bytes = encode_term_body(t, &atoms).unwrap();
        assert_eq!(&bytes[..2], &[104, 255]);
        assert_eq!(bytes.len(), 2 + 255 * 2);

        let t = heap.tuple(&vec![make_small(0); 256]).unwrap();
        let bytes = encode_term_body(t, &atoms).unwrap();
        assert_eq!(&bytes[..5], &[105, 0, 0, 1, 0]);
        assert_eq!(bytes.len(), 5 + 256 * 2);
    }

    #[test]
    fn floats_use_new_float_ext_and_ieee754_bits() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
        for value in [
            3.14_f64,
            0.0,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::MIN,
            f64::MAX,
        ] {
            let t = make_float(&mut heap, value).unwrap();
            let bytes = encode_term_body(t, &atoms).unwrap();
            assert_eq!(bytes[0], 70);
            assert_eq!(&bytes[1..], &value.to_bits().to_be_bytes());
            assert_eq!(bytes.len(), 9);
        }
    }

    #[test]
    fn maps_encode_as_map_ext() {
        let mut atoms = AtomTable::new();
        let mut heap = new_heap(64);

        let empty = make_flatmap(&mut heap, &[]).unwrap();
        assert_eq!(encode_term_body(empty, &atoms).unwrap(), vec![116, 0, 0, 0, 0]);

        let a = make_atom(atoms.intern("a"));
        let one = make_flatmap(&mut heap, &[(a, make_small(1))]).unwrap();
        assert_eq!(
            encode_term_body(one, &atoms).unwrap(),
            vec![116, 0, 0, 0, 1, 119, 1, 97, 97, 1]
        );

        let b = make_atom(atoms.intern("b"));
        let two = make_flatmap(&mut heap, &[(a, make_small(1)), (b, make_small(2))]).unwrap();
        assert_eq!(
            encode_term_body(two, &atoms).unwrap(),
            vec![116, 0, 0, 0, 2, 119, 1, 97, 97, 1, 119, 1, 98, 97, 2]
        );
    }

    #[test]
    fn external_pid_uses_new_pid_ext() {
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));
        let mut heap = new_heap(64);
        let pid = make_external_pid(&mut heap, node, 1, 2, 3).unwrap();

        let mut expected = vec![88u8];
        expected.extend_from_slice(&[119, 13]);
        expected.extend_from_slice(b"nonode@nohost");
        expected.extend_from_slice(&1u32.to_be_bytes());
        expected.extend_from_slice(&2u32.to_be_bytes());
        expected.extend_from_slice(&3u32.to_be_bytes());
        assert_eq!(encode_term_body(pid, &atoms).unwrap(), expected);
    }

    #[test]
    fn external_port_uses_new_port_ext_and_checks_the_id_width() {
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));
        let mut heap = new_heap(64);

        let port = make_external_port(&mut heap, node, 0x1234, 5).unwrap();
        let mut expected = vec![89u8];
        expected.extend_from_slice(&[119, 13]);
        expected.extend_from_slice(b"nonode@nohost");
        expected.extend_from_slice(&0x1234u32.to_be_bytes());
        expected.extend_from_slice(&5u32.to_be_bytes());
        assert_eq!(encode_term_body(port, &atoms).unwrap(), expected);

        // A 64-bit port id does not fit `NEW_PORT_EXT`'s 32-bit field: never truncate.
        let wide = make_external_port(&mut heap, node, 0x1_0000_0000, 5).unwrap();
        assert_system_limit(encode_term_body(wide, &atoms));
    }

    #[test]
    fn external_ref_uses_newer_reference_ext() {
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));
        let mut heap = new_heap(64);
        let r = make_external_ref(&mut heap, node, &[1, 2, 3], 9).unwrap();

        let mut expected = vec![90u8];
        expected.extend_from_slice(&3u16.to_be_bytes());
        expected.extend_from_slice(&[119, 13]);
        expected.extend_from_slice(b"nonode@nohost");
        expected.extend_from_slice(&9u32.to_be_bytes());
        expected.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]);
        assert_eq!(encode_term_body(r, &atoms).unwrap(), expected);
    }

    #[test]
    fn version_magic_is_emitted_only_by_new() {
        let atoms = AtomTable::new();

        let mut head = Encoder::new(&atoms);
        assert_eq!(head.as_bytes(), &[131]);
        assert_eq!(head.len(), 1);
        assert!(!head.is_empty());
        head.encode(NIL).unwrap();
        assert_eq!(head.finish(), vec![131, 106]);

        let mut body = Encoder::new_body(&atoms);
        assert!(body.is_empty());
        assert_eq!(body.len(), 0);
        assert_eq!(body.as_bytes(), &[] as &[u8]);
        body.encode(NIL).unwrap();
        assert_eq!(body.finish(), vec![106]);
    }

    #[test]
    fn nested_terms_encode_recursively() {
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let mut heap = new_heap(256);

        let inner_list = build_list(&mut heap, &[1, 2]);
        let map = make_flatmap(&mut heap, &[(a, make_small(7))]).unwrap();
        let tuple = heap.tuple(&[inner_list, map]).unwrap();

        let mut expected = vec![104u8, 2];
        expected.extend_from_slice(&[107, 0, 2, 1, 2]); // [1, 2]
        expected.extend_from_slice(&[116, 0, 0, 0, 1, 119, 1, 97, 97, 7]); // #{a => 7}
        assert_eq!(encode_term_body(tuple, &atoms).unwrap(), expected);
    }
}
