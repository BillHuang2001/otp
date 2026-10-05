//! Block layout: the per-allocation header that precedes every payload.
//!
//! Implementation module (public for testing/reuse, **not** part of the cross-crate
//! seam). Mirrors the role of the legacy `Block_t` in
//! `erts/emulator/beam/erl_alloc_util.h`: a two-word header `[bhdr][carrier]` is laid
//! down before each payload, encoding the block size, the payload offset (alignment
//! padding), the size-class index and a few flags, while giving O(1) block→carrier
//! recovery without super-alignment. Header size is [`BLOCK_HEADER_SIZE`] (16 B), so
//! payloads are `UNIT_SIZE`-aligned by construction.
//!
//! `unsafe` is confined to the marked functions below; every `unsafe fn` has a
//! `# Safety` section and every `unsafe` block a `// SAFETY:` rationale
//! (`beam_common::safety`).

use core::ffi::c_void;

use beam_term::prelude::Eterm;

/// The allocator's allocation unit (`UNIT_SIZE`): headers and payloads are
/// multiples of this many bytes, so payloads are at least this aligned.
pub const UNIT_SIZE: usize = 16;

/// Number of machine words in the fixed part of a block header.
pub const BLOCK_HEADER_WORDS: usize = 2;

/// Size in bytes of the fixed part of a block header (`UNIT_SIZE`).
pub const BLOCK_HEADER_SIZE: usize = UNIT_SIZE;

/// Size in bytes of one term word (`align_of::<Eterm>() == 8` on 64-bit).
pub const WORD_SIZE: usize = core::mem::size_of::<Eterm>();

/// The payload of this block is on its carrier's free list.
const FLAG_FREE: u8 = 1 << 0;
/// The block occupies a dedicated (large) carrier.
const FLAG_LARGE: u8 = 1 << 1;
/// The block is the last one in its carrier.
const FLAG_LAST_IN_CARRIER: u8 = 1 << 2;

/// A typed view over a block header's flag byte.
///
/// Hand-rolled bit accessors rather than the `bitflags` crate, to keep the crate's
/// dependency set at the frozen four (`beam_common`, `beam_term`, `libc`,
/// `parking_lot`).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct BlockFlags(u8);

impl BlockFlags {
    /// No flags set.
    pub const fn empty() -> BlockFlags {
        BlockFlags(0)
    }

    /// Wraps a raw flag byte.
    pub const fn from_bits(bits: u8) -> BlockFlags {
        BlockFlags(bits)
    }

    /// The raw flag byte.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether the block is currently on a free list.
    pub const fn is_free(self) -> bool {
        self.0 & FLAG_FREE != 0
    }

    /// Marks the block as free.
    pub const fn set_free(&mut self) {
        self.0 |= FLAG_FREE;
    }

    /// Clears the free bit (the block is handed out).
    pub const fn clear_free(&mut self) {
        self.0 &= !FLAG_FREE;
    }

    /// Whether the block occupies a dedicated (large) carrier.
    pub const fn is_large(self) -> bool {
        self.0 & FLAG_LARGE != 0
    }

    /// Marks the block as large (dedicated carrier).
    pub const fn set_large(&mut self) {
        self.0 |= FLAG_LARGE;
    }

    /// Whether the block is the last one in its carrier.
    pub const fn last_in_carrier(self) -> bool {
        self.0 & FLAG_LAST_IN_CARRIER != 0
    }

    /// Marks the block as the last one in its carrier.
    pub const fn set_last_in_carrier(&mut self) {
        self.0 |= FLAG_LAST_IN_CARRIER;
    }
}

/// The decoded form of a block header.
///
/// Kept deliberately minimal; the packing of these fields into the two header words
/// is finalised by the allocator implementation pass.
#[derive(Clone, Copy, Debug)]
pub struct BlockHdr {
    /// Total block size in bytes, including the header and any alignment padding.
    pub size: usize,
    /// Byte offset from the block start to the payload.
    pub payload_offset: usize,
    /// Index into the allocator's [`crate::size_class::SizeClasses`], if any.
    pub class: u16,
    /// The carrier this block was carved from (O(1) block→carrier recovery).
    pub carrier: *mut c_void,
    /// The raw flag byte (see [`BlockFlags`]).
    pub flags: u8,
}

/// The block-header codec: fixed geometry plus encode/decode of [`BlockHdr`].
pub struct BlockHeader;

impl BlockHeader {
    /// The total number of bytes the block header occupies when the payload starts
    /// `payload_offset` bytes after the block start (fixed header words plus any
    /// alignment padding).
    pub fn header_size(payload_offset: usize) -> usize {
        BLOCK_HEADER_SIZE + payload_offset
    }

    /// Writes `hdr` at the start of the block at `at`.
    ///
    /// # Safety
    ///
    /// `at` must point to at least [`BLOCK_HEADER_SIZE`] writable, `UNIT_SIZE`-aligned
    /// bytes owned by this allocator, with no live payload overlapping them.
    pub unsafe fn store(at: *mut u8, hdr: BlockHdr) {
        let _ = (at, hdr);
        unimplemented!("BlockHeader::store: header packing (allocator implementation pass)")
    }

    /// Reads the block header at `at`.
    ///
    /// # Safety
    ///
    /// `at` must point to a block previously initialised by [`BlockHeader::store`] and
    /// still owned by this allocator.
    pub unsafe fn load(at: *mut u8) -> BlockHdr {
        let _ = at;
        unimplemented!("BlockHeader::load: header unpacking (allocator implementation pass)")
    }
}
