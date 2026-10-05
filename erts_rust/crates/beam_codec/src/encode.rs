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
    ATOM_EXT, ATOM_UTF8_EXT, BINARY_EXT, BIT_BINARY_EXT, COMPRESSED_EXT, INTEGER_EXT,
    LARGE_BIG_EXT, LARGE_TUPLE_EXT, LIST_EXT, MAP_EXT, NEWER_REFERENCE_EXT, NEW_FLOAT_EXT,
    NEW_PID_EXT, NIL_EXT, SMALL_ATOM_EXT, SMALL_ATOM_UTF8_EXT, SMALL_BIG_EXT, SMALL_INTEGER_EXT,
    SMALL_TUPLE_EXT, STRING_EXT, V4_PORT_EXT, Tag,
};
use crate::term::{
    big_parts, bit_count_to_bytes, bitstring_bytes, cons_head, cons_tail, external_pid_parts,
    external_port_parts, external_ref_parts, flatmap_parts, float_value, tuple_arity, tuple_element,
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
        if let Ok((bytes, offset_bits, size_bits)) = bitstring_bytes(term) {
            return self.encode_bitstring(bytes, offset_bits, size_bits);
        }
        // Anything else is not representable by the M1 term layer (in practice an
        // off-heap `BinRef`/`Fun`/`Record` block, which the Stage-0 foundations cannot
        // build): report it explicitly rather than emit a wrong encoding. Tests never
        // reach this path.
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

    /// Encodes a flat map of **any** pair count as `MAP_EXT`.
    ///
    /// Every map is stored as a flatmap: the flatmap/hashmap switchover is an *open
    /// interior* layout (workspace `CONTEXT.md` §2) and no HAMT builder exists in the
    /// workspace yet, so there is no `MAP_SMALL_MAP_LIMIT` cutoff. The pair order is
    /// emitted as-is (interleaved `K1,V1,…,Kn,Vn`) — deliberately **not** sorted or
    /// deduped (`erl_map.h:181-199`); a future pass may sort via `beam_term::cmp`.
    /// The pair count must fit the flatmap `arityval` (24-bit), else `SystemLimit`.
    fn encode_map(&mut self, keys: &[Eterm], values: &[Eterm]) -> Result<()> {
        let n = keys.len();
        if n > MAX_ARITYVAL {
            return Err(error::system_limit());
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

    /// Consumes the encoder, compressing the written term into a `COMPRESSED`
    /// (`80`) `term_to_binary` image at zlib `level` (`0..=9`).
    ///
    /// The written term is the *inner body*: the bytes the encoder produced, minus a
    /// leading [`VERSION_MAGIC`](crate::VERSION_MAGIC) if [`Encoder::new`] wrote one
    /// (a term body never starts with `131`, which is not a tag, so stripping it when
    /// present is unambiguous). The result is
    /// `[VERSION_MAGIC][COMPRESSED_EXT][inner_size:4 BE][zlib body]` — the wire layout
    /// `erlang:term_to_binary(Term, [compressed])` produces (`external.c:2289-2297`).
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

        let mut out = Writer::with_capacity(body.len() + 6);
        out.u8(crate::VERSION_MAGIC);
        out.u8(COMPRESSED_EXT);
        out.u32be(size);
        out.bytes(&crate::compress::compress(body, level)?);
        Ok(out.into_vec())
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

/// Encodes `term` as a compressed `term_to_binary` image:
/// `[VERSION_MAGIC][COMPRESSED_EXT][inner_size:4 BE][zlib body]`.
///
/// The term is encoded uncompressed first (the `zlib` stream is the *inner* body, so
/// it never contains the version byte), then deflated at zlib `level`. This is the
/// byte layout of `erlang:term_to_binary(Term, [{compressed, Level}])`
/// (`external.c:2289-2297`), and it is exactly what [`encode_term`] would produce if
/// compression had not helped — the C falls back to the uncompressed image in that
/// case; this entry point always emits the compressed wrapper because compression was
/// explicitly requested.
///
/// The default [`encode_term`] stays uncompressed: the wrapper is emitted only here.
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
/// that body's length, version byte excluded.
pub fn encode_term_body_compressed(term: Eterm, atoms: &AtomTable, level: u8) -> Result<Vec<u8>> {
    let image = encode_term_compressed(term, atoms, level)?;
    // Drop the leading `VERSION_MAGIC`; everything else (tag, size, zlib stream) is
    // already the body form. `encode_term_compressed` guarantees the version byte.
    Ok(image[1..].to_vec())
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

        // The magnitude is little-endian, so a high-order zero **digit** contributes
        // only its (stripped) trailing byte.
        let t = make_big(&mut heap, false, &[5, 0]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![110, 1, 0, 5]);

        // A zero digit *below* the most significant one still contributes all eight of
        // its bytes (`big_to_bytes`): the value is 2^128 + 5, i.e. 17 significant
        // base-256 bytes.
        let t = make_big(&mut heap, false, &[5, 0, 1]).unwrap();
        let mut expected = vec![110u8, 17, 0, 5];
        expected.extend(core::iter::repeat_n(0u8, 15));
        expected.push(1);
        assert_eq!(encode_term_body(t, &atoms).unwrap(), expected);

        // An all-zero magnitude coincides with the small integer 0 on the wire.
        let t = make_big(&mut heap, false, &[0]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![97, 0]);
        let t = make_big(&mut heap, false, &[0, 0]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![97, 0]);
    }

    #[test]
    fn digit_bytes_are_little_endian_with_a_short_top_digit() {
        assert_eq!(digits_to_le_bytes(&[]), Vec::<u8>::new());
        assert_eq!(digits_to_le_bytes(&[1]), vec![1]);
        assert_eq!(digits_to_le_bytes(&[0x0102]), vec![0x02, 0x01]);
        assert_eq!(
            digits_to_le_bytes(&[0x0102, 0x03]),
            vec![0x02, 0x01, 0, 0, 0, 0, 0, 0, 0x03]
        );
        assert_eq!(digits_to_le_bytes(&[0, 0]), vec![0; 8 + 1]);
        assert_eq!(digits_to_le_bytes(&[u64::MAX]), vec![0xFF; 8]);
    }

    #[test]
    fn bignum_with_at_least_256_bytes_uses_large_big() {
        let atoms = AtomTable::new();
        // 32 words of `0x01` bytes = 256 significant base-256 bytes.
        let digits = vec![0x0101_0101_0101_0101u64; 32];
        let mut heap = new_heap(4096);
        let t = make_big(&mut heap, false, &digits).unwrap();

        let bytes = encode_term_body(t, &atoms).unwrap();
        assert_eq!(bytes[0], 111);
        assert_eq!(&bytes[1..5], &[0, 0, 1, 0]); // 256, big-endian u32
        assert_eq!(bytes[5], 0); // sign
        assert_eq!(&bytes[6..], &vec![1u8; 256][..]);
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
    fn maps_above_the_small_limit_encode_as_map_ext() {
        // 40 pairs — past `MAP_SMALL_MAP_LIMIT` (32) — is still a `MAP_EXT` flatmap,
        // never `unsupported` (the flatmap/hashmap switchover is an open interior
        // layout; no HAMT builder exists).
        let mut atoms = AtomTable::new();
        let mut heap = new_heap(4 + 2 * 40 + 64);
        let mut pairs = Vec::new();
        for i in 0..40i64 {
            let key = make_atom(atoms.intern(&format!("k{i}")));
            pairs.push((key, make_small(i)));
        }
        let map = make_flatmap(&mut heap, &pairs).unwrap();

        let bytes = encode_term_body(map, &atoms).unwrap();
        assert_eq!(bytes[0], 116); // MAP_EXT
        assert_eq!(u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]), 40);
        // The wire order is the pair order: `K1,V1,…,Kn,Vn` (name then value).
        assert_eq!(&bytes[5..9], &[119, 2, b'k', b'0']); // "k0"
        assert_eq!(bytes[9], 97); // small integer
        assert_eq!(bytes[10], 0);
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
    fn external_port_uses_v4_port_ext_with_the_full_id() {
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));
        let mut heap = new_heap(64);

        let expected = |number: u64, creation: u32| {
            let mut e = vec![120u8];
            e.extend_from_slice(&[119, 13]);
            e.extend_from_slice(b"nonode@nohost");
            e.extend_from_slice(&number.to_be_bytes());
            e.extend_from_slice(&creation.to_be_bytes());
            e
        };

        // A small id still uses the 64-bit `V4_PORT_EXT` form (never `NEW_PORT_EXT`).
        let port = make_external_port(&mut heap, node, 0x1234, 5).unwrap();
        assert_eq!(encode_term_body(port, &atoms).unwrap(), expected(0x1234, 5));

        // A 64-bit port id is written in full — never truncated, never a limit.
        let wide = make_external_port(&mut heap, node, 0x1_0000_0000, 5).unwrap();
        assert_eq!(
            encode_term_body(wide, &atoms).unwrap(),
            expected(0x1_0000_0000, 5)
        );

        let max = make_external_port(&mut heap, node, u64::MAX, u32::MAX).unwrap();
        assert_eq!(
            encode_term_body(max, &atoms).unwrap(),
            expected(u64::MAX, u32::MAX)
        );
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
