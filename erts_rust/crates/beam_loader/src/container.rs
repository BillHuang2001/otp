//! The `FOR1` container: the outer `.beam` file framing and its chunk directory.
//!
//! Wire layout (verified against the OTP 29 / erts-17.1 oracle):
//!
//! ```text
//! u32  "FOR1"                        // iff magic
//! u32  form_length                   // = file_size - 8 ; INCLUDES the 4-byte form type
//! u32  "BEAM"                        // iff FORM TYPE (not a chunk)
//! chunk entry * :  u8[4] chunk_id | u32 size | size bytes | (4 - size%4)%4 zero pad
//! ```
//!
//! `"BEAM"` is the form *type*, not a chunk. Chunks parse until EOF; unknown ids
//! are skipped, duplicate ids keep the first, and chunk order is not significant.
//!
//! Parsing is safe Rust: the byte cursor is [`beam_codec::Reader`], the ETF
//! codec's bounds-checked big-endian reader (the lowest common ancestor for
//! big-endian reads) — there is no second cursor and no raw pointer in this crate.
use beam_common::Result;

/// A four-byte chunk identifier (e.g. `b"AtU8"`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ChunkId(pub [u8; 4]);

impl ChunkId {
    /// The atom table chunk (`AtU8`, UTF-8 atom names).
    ///
    /// The obsolete Latin-1 `Atom` chunk id is deliberately not a constant: it is
    /// superseded by `AtU8` and, if encountered, is treated as an unknown chunk.
    pub const ATOM: ChunkId = ChunkId(*b"AtU8");
    /// The instruction stream chunk (`Code`).
    pub const CODE: ChunkId = ChunkId(*b"Code");
    /// The string table chunk (`StrT`).
    pub const STRT: ChunkId = ChunkId(*b"StrT");
    /// The import table chunk (`ImpT`).
    pub const IMPT: ChunkId = ChunkId(*b"ImpT");
    /// The export table chunk (`ExpT`).
    pub const EXPT: ChunkId = ChunkId(*b"ExpT");
    /// The fun/lambda table chunk (`FunT`).
    pub const FUNT: ChunkId = ChunkId(*b"FunT");
    /// The literal table chunk (`LitT`).
    pub const LITT: ChunkId = ChunkId(*b"LitT");
    /// The module attributes chunk (`Attr`).
    pub const ATTR: ChunkId = ChunkId(*b"Attr");
    /// The compile-info chunk (`CInf`).
    pub const CINF: ChunkId = ChunkId(*b"CInf");
    /// The line table chunk (`Line`).
    pub const LINE: ChunkId = ChunkId(*b"Line");
    /// The JIT-only local table chunk (`LocT`); not parsed by the interpreter path.
    pub const LOCT: ChunkId = ChunkId(*b"LocT");
    /// The type-annotation chunk (`Type`).
    pub const TYPE: ChunkId = ChunkId(*b"Type");
    /// The metadata chunk (`Meta`).
    pub const META: ChunkId = ChunkId(*b"Meta");
    /// The debug-info chunk (`DbgB`).
    pub const DBGB: ChunkId = ChunkId(*b"DbgB");
    /// The (obsolete) native-record chunk (`Recs`).
    pub const RECS: ChunkId = ChunkId(*b"Recs");

    /// Builds a `ChunkId` from its four raw bytes.
    pub fn new(bytes: [u8; 4]) -> ChunkId {
        ChunkId(bytes)
    }

    /// The four raw bytes of this chunk id.
    pub fn name(self) -> [u8; 4] {
        self.0
    }
}

/// One raw chunk: its id plus a borrowed view of its (unpadded) payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawChunk<'a> {
    /// The chunk identifier.
    pub id: ChunkId,
    /// The chunk payload (`size` bytes, padding excluded).
    pub data: &'a [u8],
}

/// The parsed container: the file's chunks in file order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Container<'a> {
    /// The chunks, in the order they appear in the file.
    pub chunks: Vec<RawChunk<'a>>,
}

impl<'a> Container<'a> {
    /// The first chunk with the given id, if any (duplicate ids keep the first).
    pub fn find(&self, id: ChunkId) -> Option<&RawChunk<'a>> {
        self.chunks.iter().find(|chunk| chunk.id == id)
    }
}

/// Parses the `FOR1` header and the chunk directory out of a `.beam` image.
///
/// A missing/bad `FOR1` magic, a bad form type, a chunk size that overruns the file
/// or a truncated directory each yield `BeamError::BadArg` — never a panic.
// TODO(beam_loader-impl): read the `FOR1` magic, `form_length` and the `BEAM` form
// TODO(beam_loader-impl): type, then walk the chunk directory (id + size + payload +
// TODO(beam_loader-impl): 4-byte alignment padding) until EOF, collecting `RawChunk`s.
pub fn parse(bytes: &[u8]) -> Result<Container<'_>> {
    let _ = bytes;
    Err(beam_common::BeamError::BadArg)
}
