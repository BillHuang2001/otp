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
pub const COMPRESSED_EXT: u8 = 80;

/// `BIT_BINARY_EXT` (`'M'`, `external.h:52`): a non-byte-aligned bitstring.
pub const BIT_BINARY_EXT: u8 = 77;

/// `NEW_FUN_EXT` (`'p'`, `external.h:55`): a fun reference.
pub const NEW_FUN_EXT: u8 = 112;

/// `EXPORT_EXT` (`'q'`, `external.h:56`): an exported fun.
pub const EXPORT_EXT: u8 = 113;

/// `FUN_EXT` (`'u'`, `external.h:58`): the legacy fun form.
pub const FUN_EXT: u8 = 117;

/// `V4_PORT_EXT` (`'x'`, `external.h:61`): a port with a 64-bit id.
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
/// update. Owned by `beam_dist`; M1 never emits it.
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
        unimplemented!()
    }

    /// The wire byte for this tag.
    pub fn as_byte(self) -> u8 {
        unimplemented!()
    }

    /// The canonical name of this tag: the C macro spelling from `external.h`
    /// (e.g. `"SMALL_INTEGER_EXT"`, `"COMPRESSED"`, `"ATOM_CACHE_REF"`).
    pub fn name(self) -> &'static str {
        unimplemented!()
    }

    /// Whether the Milestone-1 term layer supports this tag.
    ///
    /// `true` for the required core set (integers, floats, all four atom tags,
    /// references, ports, pids, tuples, `NIL`, strings, lists, small maps and the
    /// distribution atom-cache reference); `false` for everything the codec
    /// deliberately reports as [`crate::error::unsupported`] (`COMPRESSED`,
    /// binaries, funs, records, `V4_PORT_EXT`, `LOCAL_EXT` and the distribution
    /// header).
    pub fn covered(self) -> bool {
        unimplemented!()
    }
}
