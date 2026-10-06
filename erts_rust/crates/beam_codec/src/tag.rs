//! Wire-frozen External Term Format tag values.
//!
//! Every constant in this module is copied verbatim from the legacy C header
//! `erts/emulator/beam/external.h`. These bytes are part of the distribution wire
//! protocol (compatibility contract A.5) and **must not change**; each doc comment
//! records the originating C macro and its line in `external.h`.
//!
//! [`Tag`] is the type-safe companion used by the reader/writer and
//! encode/decode layers; [`Tag::covered`] marks the tags the Milestone-1 term
//! layer supports.

/// The external term format version byte (`external.h:84`).
///
/// Emitted first by `term_to_binary/1` and required by `binary_to_term/1`; it is
/// **not** part of a distribution payload that follows a distribution header.
pub const VERSION_MAGIC: u8 = 131;

/// `SMALL_INTEGER_EXT` (`'a'`, `external.h:33`): an unsigned integer in `0..=255`.
pub const SMALL_INTEGER_EXT: u8 = 97;

/// `INTEGER_EXT` (`'b'`, `external.h:34`): a signed 32-bit big-endian integer.
pub const INTEGER_EXT: u8 = 98;

/// `FLOAT_EXT` (`'c'`, `external.h:35`): a legacy float written as a
/// `%.20e` NUL-terminated ASCII string (decoded, never emitted).
pub const FLOAT_EXT: u8 = 99;

/// `ATOM_EXT` (`'d'`, `external.h:36`): a Latin-1 atom with a 16-bit length.
pub const ATOM_EXT: u8 = 100;

/// `REFERENCE_EXT` (`'e'`, `external.h:38`): the legacy reference form.
pub const REFERENCE_EXT: u8 = 101;

/// `PORT_EXT` (`'f'`, `external.h:41`): the legacy port form.
pub const PORT_EXT: u8 = 102;

/// `PID_EXT` (`'g'`, `external.h:44`): the legacy pid form.
pub const PID_EXT: u8 = 103;

/// `SMALL_TUPLE_EXT` (`'h'`, `external.h:46`): a tuple with an 8-bit arity.
pub const SMALL_TUPLE_EXT: u8 = 104;

/// `LARGE_TUPLE_EXT` (`'i'`, `external.h:47`): a tuple with a 32-bit arity.
pub const LARGE_TUPLE_EXT: u8 = 105;

/// `NIL_EXT` (`'j'`, `external.h:48`): the empty list.
pub const NIL_EXT: u8 = 106;

/// `STRING_EXT` (`'k'`, `external.h:49`): a byte list with a 16-bit length.
pub const STRING_EXT: u8 = 107;

/// `LIST_EXT` (`'l'`, `external.h:50`): a cons list (head plus tail).
pub const LIST_EXT: u8 = 108;

/// `BINARY_EXT` (`'m'`, `external.h:51`): a byte-aligned binary.
pub const BINARY_EXT: u8 = 109;

/// `SMALL_BIG_EXT` (`'n'`, `external.h:53`): a bignum with an 8-bit digit count.
pub const SMALL_BIG_EXT: u8 = 110;

/// `LARGE_BIG_EXT` (`'o'`, `external.h:54`): a bignum with a 32-bit digit count.
pub const LARGE_BIG_EXT: u8 = 111;

/// `NEW_FLOAT_EXT` (`'F'`, `external.h:43`): an IEEE-754 double, 8 bytes big-endian.
pub const NEW_FLOAT_EXT: u8 = 70;

/// `NEW_REFERENCE_EXT` (`'r'`, `external.h:39`): a reference with 32-bit ids.
pub const NEW_REFERENCE_EXT: u8 = 114;

/// `SMALL_ATOM_EXT` (`'s'`, `external.h:37`): a Latin-1 atom with an 8-bit length.
pub const SMALL_ATOM_EXT: u8 = 115;

/// `MAP_EXT` (`'t'`, `external.h:57`): a map with a 32-bit pair count.
pub const MAP_EXT: u8 = 116;

/// `NEW_PID_EXT` (`'X'`, `external.h:45`): the modern pid form.
pub const NEW_PID_EXT: u8 = 88;

/// `NEW_PORT_EXT` (`'Y'`, `external.h:42`): the modern port form.
pub const NEW_PORT_EXT: u8 = 89;

/// `NEWER_REFERENCE_EXT` (`'Z'`, `external.h:40`): the modern reference form.
pub const NEWER_REFERENCE_EXT: u8 = 90;

/// `ATOM_UTF8_EXT` (`'v'`, `external.h:59`): a UTF-8 atom with a 16-bit length.
pub const ATOM_UTF8_EXT: u8 = 118;

/// `SMALL_ATOM_UTF8_EXT` (`'w'`, `external.h:60`): a UTF-8 atom with an 8-bit length.
pub const SMALL_ATOM_UTF8_EXT: u8 = 119;

/// `COMPRESSED` (`'P'`, `external.h:75`): a zlib-compressed payload.
///
/// The wire form is `[VERSION_MAGIC][COMPRESSED_EXT][uncompressed_size:4 BE][zlib
/// stream]`, valid only as the outermost wrapper (immediately after the version byte);
/// [`crate::decode::decode_term`] unwraps it. See [`crate::compress`].
pub const COMPRESSED_EXT: u8 = 80;

/// `BIT_BINARY_EXT` (`'M'`, `external.h:52`): a non-byte-aligned bitstring.
pub const BIT_BINARY_EXT: u8 = 77;

/// `NEW_FUN_EXT` (`'p'`, `external.h:55`): a fun reference.
pub const NEW_FUN_EXT: u8 = 112;

/// `EXPORT_EXT` (`'q'`, `external.h:56`): an exported fun.
pub const EXPORT_EXT: u8 = 113;

/// `FUN_EXT` (`'u'`, `external.h:58`): the legacy (pre-OTP 23) fun form.
///
/// Removed in OTP 23; the C decoder rejects it (`goto error`, `external.c:6399-6404`),
/// so byte `117` is **malformed**, never a term and never `unsupported`.
pub const FUN_EXT: u8 = 117;

/// `V4_PORT_EXT` (`'x'`, `external.h:61`): a port with a 64-bit id.
///
/// The C encoder **always** emits this form for a port (`external.c:3675`/`:3684`);
/// `NEW_PORT_EXT` (`89`) is decode-only.
pub const V4_PORT_EXT: u8 = 120;

/// `LOCAL_EXT` (`'y'`, `external.h:62`): a local (node-less) pid/port/reference.
pub const LOCAL_EXT: u8 = 121;

/// `ATOM_CACHE_REF` (`'R'`, `external.h:69`): a one-byte distribution atom-cache
/// reference, resolved through [`crate::atom_cache::AtomCache`].
pub const ATOM_CACHE_REF_EXT: u8 = 82;

/// `DIST_HEADER` (`'D'`, `external.h:65`): the distribution header. Owned by
/// `beam_dist`, which also fills the atom cache it carries.
pub const DIST_HEADER_EXT: u8 = 68;

/// `NEW_CACHE` (`'N'`, `external.h:80`): the extended distribution atom-cache
/// update.
///
/// **Dead in this OTP** (`#if 0`, `external.h:77-81`): byte `78` is
/// `MAGIC_REF_INTERNAL_REF` (`external.h:74`), not a term tag. Reaching it in a term
/// is **malformed**, never `unsupported`.
pub const NEW_CACHE_EXT: u8 = 78;

/// `RECORD_EXT` (`'C'`, `external.h:63`): a tuple tagged as a record.
pub const RECORD_EXT: u8 = 67;

/// The External Term Format tags, keyed by their wire byte.
///
/// One variant per tag in `external.h`; see the module docs and the individual
/// constants for the exact byte, C macro name and `external.h` line.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tag {
    /// `SMALL_INTEGER_EXT` (`97`).
    SmallInteger,
    /// `INTEGER_EXT` (`98`).
    Integer,
    /// `FLOAT_EXT` (`99`, legacy ASCII float).
    Float,
    /// `ATOM_EXT` (`100`, Latin-1).
    Atom,
    /// `REFERENCE_EXT` (`101`, legacy).
    Reference,
    /// `PORT_EXT` (`102`, legacy).
    Port,
    /// `PID_EXT` (`103`, legacy).
    Pid,
    /// `SMALL_TUPLE_EXT` (`104`).
    SmallTuple,
    /// `LARGE_TUPLE_EXT` (`105`).
    LargeTuple,
    /// `NIL_EXT` (`106`).
    Nil,
    /// `STRING_EXT` (`107`).
    String,
    /// `LIST_EXT` (`108`).
    List,
    /// `BINARY_EXT` (`109`).
    Binary,
    /// `SMALL_BIG_EXT` (`110`).
    SmallBig,
    /// `LARGE_BIG_EXT` (`111`).
    LargeBig,
    /// `NEW_FLOAT_EXT` (`70`, IEEE-754 double).
    NewFloat,
    /// `NEW_REFERENCE_EXT` (`114`).
    NewReference,
    /// `SMALL_ATOM_EXT` (`115`, Latin-1).
    SmallAtom,
    /// `MAP_EXT` (`116`).
    Map,
    /// `NEW_PID_EXT` (`88`).
    NewPid,
    /// `NEW_PORT_EXT` (`89`).
    NewPort,
    /// `NEWER_REFERENCE_EXT` (`90`).
    NewerReference,
    /// `ATOM_UTF8_EXT` (`118`).
    AtomUtf8,
    /// `SMALL_ATOM_UTF8_EXT` (`119`).
    SmallAtomUtf8,
    /// `COMPRESSED` (`80`).
    ///
    /// Honoured only as the top-level wrapper (right after the version byte); nested
    /// it is malformed input. See [`Tag::covered`].
    Compressed,
    /// `BIT_BINARY_EXT` (`77`).
    BitBinary,
    /// `NEW_FUN_EXT` (`112`).
    NewFun,
    /// `EXPORT_EXT` (`113`).
    Export,
    /// `FUN_EXT` (`117`, legacy).
    Fun,
    /// `V4_PORT_EXT` (`120`).
    V4Port,
    /// `LOCAL_EXT` (`121`).
    Local,
    /// `ATOM_CACHE_REF` (`82`).
    AtomCacheRef,
    /// `DIST_HEADER` (`68`).
    DistHeader,
    /// `NEW_CACHE` (`78`).
    NewCache,
    /// `RECORD_EXT` (`67`).
    Record,
}

impl Tag {
    /// Decodes a wire byte into a [`Tag`], or `None` if it is not a known tag.
    pub fn from_byte(byte: u8) -> Option<Tag> {
        let tag = match byte {
            SMALL_INTEGER_EXT => Tag::SmallInteger,
            INTEGER_EXT => Tag::Integer,
            FLOAT_EXT => Tag::Float,
            ATOM_EXT => Tag::Atom,
            REFERENCE_EXT => Tag::Reference,
            PORT_EXT => Tag::Port,
            PID_EXT => Tag::Pid,
            SMALL_TUPLE_EXT => Tag::SmallTuple,
            LARGE_TUPLE_EXT => Tag::LargeTuple,
            NIL_EXT => Tag::Nil,
            STRING_EXT => Tag::String,
            LIST_EXT => Tag::List,
            BINARY_EXT => Tag::Binary,
            SMALL_BIG_EXT => Tag::SmallBig,
            LARGE_BIG_EXT => Tag::LargeBig,
            NEW_FLOAT_EXT => Tag::NewFloat,
            NEW_REFERENCE_EXT => Tag::NewReference,
            SMALL_ATOM_EXT => Tag::SmallAtom,
            MAP_EXT => Tag::Map,
            NEW_PID_EXT => Tag::NewPid,
            NEW_PORT_EXT => Tag::NewPort,
            NEWER_REFERENCE_EXT => Tag::NewerReference,
            ATOM_UTF8_EXT => Tag::AtomUtf8,
            SMALL_ATOM_UTF8_EXT => Tag::SmallAtomUtf8,
            COMPRESSED_EXT => Tag::Compressed,
            BIT_BINARY_EXT => Tag::BitBinary,
            NEW_FUN_EXT => Tag::NewFun,
            EXPORT_EXT => Tag::Export,
            FUN_EXT => Tag::Fun,
            V4_PORT_EXT => Tag::V4Port,
            LOCAL_EXT => Tag::Local,
            ATOM_CACHE_REF_EXT => Tag::AtomCacheRef,
            DIST_HEADER_EXT => Tag::DistHeader,
            NEW_CACHE_EXT => Tag::NewCache,
            RECORD_EXT => Tag::Record,
            _ => return None,
        };
        Some(tag)
    }

    /// The wire byte for this tag.
    pub fn as_byte(self) -> u8 {
        match self {
            Tag::SmallInteger => SMALL_INTEGER_EXT,
            Tag::Integer => INTEGER_EXT,
            Tag::Float => FLOAT_EXT,
            Tag::Atom => ATOM_EXT,
            Tag::Reference => REFERENCE_EXT,
            Tag::Port => PORT_EXT,
            Tag::Pid => PID_EXT,
            Tag::SmallTuple => SMALL_TUPLE_EXT,
            Tag::LargeTuple => LARGE_TUPLE_EXT,
            Tag::Nil => NIL_EXT,
            Tag::String => STRING_EXT,
            Tag::List => LIST_EXT,
            Tag::Binary => BINARY_EXT,
            Tag::SmallBig => SMALL_BIG_EXT,
            Tag::LargeBig => LARGE_BIG_EXT,
            Tag::NewFloat => NEW_FLOAT_EXT,
            Tag::NewReference => NEW_REFERENCE_EXT,
            Tag::SmallAtom => SMALL_ATOM_EXT,
            Tag::Map => MAP_EXT,
            Tag::NewPid => NEW_PID_EXT,
            Tag::NewPort => NEW_PORT_EXT,
            Tag::NewerReference => NEWER_REFERENCE_EXT,
            Tag::AtomUtf8 => ATOM_UTF8_EXT,
            Tag::SmallAtomUtf8 => SMALL_ATOM_UTF8_EXT,
            Tag::Compressed => COMPRESSED_EXT,
            Tag::BitBinary => BIT_BINARY_EXT,
            Tag::NewFun => NEW_FUN_EXT,
            Tag::Export => EXPORT_EXT,
            Tag::Fun => FUN_EXT,
            Tag::V4Port => V4_PORT_EXT,
            Tag::Local => LOCAL_EXT,
            Tag::AtomCacheRef => ATOM_CACHE_REF_EXT,
            Tag::DistHeader => DIST_HEADER_EXT,
            Tag::NewCache => NEW_CACHE_EXT,
            Tag::Record => RECORD_EXT,
        }
    }

    /// The canonical name of this tag: the C macro spelling from `external.h`
    /// (e.g. `"SMALL_INTEGER_EXT"`, `"COMPRESSED"`, `"ATOM_CACHE_REF"`).
    pub fn name(self) -> &'static str {
        match self {
            Tag::SmallInteger => "SMALL_INTEGER_EXT",
            Tag::Integer => "INTEGER_EXT",
            Tag::Float => "FLOAT_EXT",
            Tag::Atom => "ATOM_EXT",
            Tag::Reference => "REFERENCE_EXT",
            Tag::Port => "PORT_EXT",
            Tag::Pid => "PID_EXT",
            Tag::SmallTuple => "SMALL_TUPLE_EXT",
            Tag::LargeTuple => "LARGE_TUPLE_EXT",
            Tag::Nil => "NIL_EXT",
            Tag::String => "STRING_EXT",
            Tag::List => "LIST_EXT",
            Tag::Binary => "BINARY_EXT",
            Tag::SmallBig => "SMALL_BIG_EXT",
            Tag::LargeBig => "LARGE_BIG_EXT",
            Tag::NewFloat => "NEW_FLOAT_EXT",
            Tag::NewReference => "NEW_REFERENCE_EXT",
            Tag::SmallAtom => "SMALL_ATOM_EXT",
            Tag::Map => "MAP_EXT",
            Tag::NewPid => "NEW_PID_EXT",
            Tag::NewPort => "NEW_PORT_EXT",
            Tag::NewerReference => "NEWER_REFERENCE_EXT",
            Tag::AtomUtf8 => "ATOM_UTF8_EXT",
            Tag::SmallAtomUtf8 => "SMALL_ATOM_UTF8_EXT",
            Tag::Compressed => "COMPRESSED",
            Tag::BitBinary => "BIT_BINARY_EXT",
            Tag::NewFun => "NEW_FUN_EXT",
            Tag::Export => "EXPORT_EXT",
            Tag::Fun => "FUN_EXT",
            Tag::V4Port => "V4_PORT_EXT",
            Tag::Local => "LOCAL_EXT",
            Tag::AtomCacheRef => "ATOM_CACHE_REF",
            Tag::DistHeader => "DIST_HEADER",
            Tag::NewCache => "NEW_CACHE",
            Tag::Record => "RECORD_EXT",
        }
    }

    /// Whether the Milestone-1 term layer supports this tag.
    ///
    /// `true` for the required core set (integers, floats, all four atom tags,
    /// references, ports — including the modern `V4_PORT_EXT` — pids, tuples, `NIL`,
    /// strings, lists, binaries/bitstrings, maps of any pair count, funs/export
    /// literals, the distribution atom-cache reference, and `COMPRESSED` as a
    /// top-level wrapper); `false` for everything the codec deliberately reports as
    /// [`crate::error::unsupported`] (records, `LOCAL_EXT` and the distribution
    /// header) **or** rejects as malformed ([`Tag::Fun`], [`Tag::NewCache`]).
    ///
    /// `COMPRESSED` (`80`) is reported as covered because the codec does honour it —
    /// but only as the outermost wrapper, immediately after the version byte
    /// (`crate::decode::decode_term`); nested it is malformed input.
    ///
    /// `NEW_FUN_EXT`/`EXPORT_EXT` are covered: they decode to fun terms and encode
    /// back byte-exactly (see `crate::decode`/`crate::encode`).
    pub fn covered(self) -> bool {
        matches!(
            self,
            Tag::SmallInteger
                | Tag::Integer
                | Tag::Float
                | Tag::NewFloat
                | Tag::Atom
                | Tag::SmallAtom
                | Tag::AtomUtf8
                | Tag::SmallAtomUtf8
                | Tag::Reference
                | Tag::NewReference
                | Tag::NewerReference
                | Tag::Port
                | Tag::NewPort
                | Tag::V4Port
                | Tag::Pid
                | Tag::NewPid
                | Tag::SmallTuple
                | Tag::LargeTuple
                | Tag::Nil
                | Tag::String
                | Tag::List
                | Tag::SmallBig
                | Tag::LargeBig
                | Tag::Binary
                | Tag::BitBinary
                | Tag::Map
                | Tag::NewFun
                | Tag::Export
                | Tag::AtomCacheRef
                | Tag::Compressed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every [`Tag`] variant with its wire byte, C macro name and M1 coverage.
    ///
    /// Ordering is the declaration order of [`Tag`]; `ALL.len()` is asserted
    /// against the variant count so a new variant forces this table to be updated.
    const ALL: [(Tag, u8, &str, bool); 35] = [
        (Tag::SmallInteger, 97, "SMALL_INTEGER_EXT", true),
        (Tag::Integer, 98, "INTEGER_EXT", true),
        (Tag::Float, 99, "FLOAT_EXT", true),
        (Tag::Atom, 100, "ATOM_EXT", true),
        (Tag::Reference, 101, "REFERENCE_EXT", true),
        (Tag::Port, 102, "PORT_EXT", true),
        (Tag::Pid, 103, "PID_EXT", true),
        (Tag::SmallTuple, 104, "SMALL_TUPLE_EXT", true),
        (Tag::LargeTuple, 105, "LARGE_TUPLE_EXT", true),
        (Tag::Nil, 106, "NIL_EXT", true),
        (Tag::String, 107, "STRING_EXT", true),
        (Tag::List, 108, "LIST_EXT", true),
        (Tag::Binary, 109, "BINARY_EXT", true),
        (Tag::SmallBig, 110, "SMALL_BIG_EXT", true),
        (Tag::LargeBig, 111, "LARGE_BIG_EXT", true),
        (Tag::NewFloat, 70, "NEW_FLOAT_EXT", true),
        (Tag::NewReference, 114, "NEW_REFERENCE_EXT", true),
        (Tag::SmallAtom, 115, "SMALL_ATOM_EXT", true),
        (Tag::Map, 116, "MAP_EXT", true),
        (Tag::NewPid, 88, "NEW_PID_EXT", true),
        (Tag::NewPort, 89, "NEW_PORT_EXT", true),
        (Tag::NewerReference, 90, "NEWER_REFERENCE_EXT", true),
        (Tag::AtomUtf8, 118, "ATOM_UTF8_EXT", true),
        (Tag::SmallAtomUtf8, 119, "SMALL_ATOM_UTF8_EXT", true),
        (Tag::Compressed, 80, "COMPRESSED", true),
        (Tag::BitBinary, 77, "BIT_BINARY_EXT", true),
        (Tag::NewFun, 112, "NEW_FUN_EXT", true),
        (Tag::Export, 113, "EXPORT_EXT", true),
        (Tag::Fun, 117, "FUN_EXT", false),
        (Tag::V4Port, 120, "V4_PORT_EXT", true),
        (Tag::Local, 121, "LOCAL_EXT", false),
        (Tag::AtomCacheRef, 82, "ATOM_CACHE_REF", true),
        (Tag::DistHeader, 68, "DIST_HEADER", false),
        (Tag::NewCache, 78, "NEW_CACHE", false),
        (Tag::Record, 67, "RECORD_EXT", false),
    ];

    #[test]
    fn version_magic_is_131() {
        assert_eq!(VERSION_MAGIC, 131);
        // `VERSION_MAGIC` is not a term tag: it never starts a term payload.
        assert_eq!(Tag::from_byte(VERSION_MAGIC), None);
    }

    #[test]
    fn constants_have_documented_byte_values() {
        assert_eq!(SMALL_INTEGER_EXT, 97);
        assert_eq!(INTEGER_EXT, 98);
        assert_eq!(FLOAT_EXT, 99);
        assert_eq!(ATOM_EXT, 100);
        assert_eq!(REFERENCE_EXT, 101);
        assert_eq!(PORT_EXT, 102);
        assert_eq!(PID_EXT, 103);
        assert_eq!(SMALL_TUPLE_EXT, 104);
        assert_eq!(LARGE_TUPLE_EXT, 105);
        assert_eq!(NIL_EXT, 106);
        assert_eq!(STRING_EXT, 107);
        assert_eq!(LIST_EXT, 108);
        assert_eq!(BINARY_EXT, 109);
        assert_eq!(SMALL_BIG_EXT, 110);
        assert_eq!(LARGE_BIG_EXT, 111);
        assert_eq!(NEW_FLOAT_EXT, 70);
        assert_eq!(NEW_REFERENCE_EXT, 114);
        assert_eq!(SMALL_ATOM_EXT, 115);
        assert_eq!(MAP_EXT, 116);
        assert_eq!(NEW_PID_EXT, 88);
        assert_eq!(NEW_PORT_EXT, 89);
        assert_eq!(NEWER_REFERENCE_EXT, 90);
        assert_eq!(ATOM_UTF8_EXT, 118);
        assert_eq!(SMALL_ATOM_UTF8_EXT, 119);
        assert_eq!(COMPRESSED_EXT, 80);
        assert_eq!(BIT_BINARY_EXT, 77);
        assert_eq!(NEW_FUN_EXT, 112);
        assert_eq!(EXPORT_EXT, 113);
        assert_eq!(FUN_EXT, 117);
        assert_eq!(V4_PORT_EXT, 120);
        assert_eq!(LOCAL_EXT, 121);
        assert_eq!(ATOM_CACHE_REF_EXT, 82);
        assert_eq!(DIST_HEADER_EXT, 68);
        assert_eq!(NEW_CACHE_EXT, 78);
        assert_eq!(RECORD_EXT, 67);
    }

    #[test]
    fn as_byte_matches_the_wire_byte() {
        for (tag, byte, _, _) in ALL {
            assert_eq!(tag.as_byte(), byte, "as_byte mismatch for {tag:?}");
        }
    }

    #[test]
    fn from_byte_inverts_as_byte() {
        for (tag, byte, _, _) in ALL {
            assert_eq!(Tag::from_byte(byte), Some(tag), "from_byte mismatch for {byte}");
            assert_eq!(
                Tag::from_byte(tag.as_byte()),
                Some(tag),
                "from_byte/as_byte round-trip mismatch for {tag:?}"
            );
        }
    }

    #[test]
    fn as_byte_is_injective() {
        for (i, (tag_a, byte_a, _, _)) in ALL.iter().enumerate() {
            for (tag_b, byte_b, _, _) in ALL.iter().skip(i + 1) {
                assert_ne!(
                    byte_a, byte_b,
                    "{tag_a:?} and {tag_b:?} share the wire byte {byte_a}"
                );
            }
        }
    }

    #[test]
    fn names_are_the_c_macro_spellings() {
        for (tag, _, name, _) in ALL {
            assert_eq!(tag.name(), name, "name mismatch for {tag:?}");
        }
    }

    #[test]
    fn covered_matches_the_m1_tag_set() {
        for (tag, _, name, covered) in ALL {
            assert_eq!(
                tag.covered(),
                covered,
                "covered mismatch for {name} ({tag:?})"
            );
        }
    }

    #[test]
    fn unknown_bytes_are_rejected() {
        assert_eq!(Tag::from_byte(0x00), None);
        assert_eq!(Tag::from_byte(200), None);
        assert_eq!(Tag::from_byte(0xFF), None);
        // Bytes adjacent to real tags stay unknown.
        assert_eq!(Tag::from_byte(65), None);
        assert_eq!(Tag::from_byte(126), None);
    }
}
