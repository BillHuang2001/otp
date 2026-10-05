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
//! # The `bhdr` word
//!
//! ```text
//! 63                        32 31        20 19        8 7          0
//! +---------------------------+------------+------------+------------+
//! | size / UNIT_SIZE  (32 bit)| off/16 (12)| class (12) | flags (8)  |
//! +---------------------------+------------+------------+------------+
//! ```
//!
//! * `size` is the **total** block size in bytes (header + alignment padding +
//!   payload), stored in [`UNIT_SIZE`] units: every block size the allocator creates
//!   is a multiple of `UNIT_SIZE`, so the packing is lossless.
//! * `off` is [`BlockHdr::payload_offset`] in `UNIT_SIZE` units (alignment padding
//!   between the fixed header and the payload).
//! * `class` is the [`crate::size_class::SizeClasses`] index of the block's size
//!   class, or the [`CLASS_NONE`] sentinel for a dedicated (large) carrier block.
//! * `flags` is the raw [`BlockFlags`] byte.
//!
//! The second header word holds the owning carrier's base address, which is what
//! makes block→carrier recovery O(1) without super-aligning the payload.
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

/// Every flag bit this module defines; a header whose flag byte has any other bit
/// set is corrupt (used by the allocator's header plausibility check).
pub const ALL_FLAGS: u8 = FLAG_FREE | FLAG_LARGE | FLAG_LAST_IN_CARRIER;

/// The `class` sentinel of a block that has no size class: a block carved from a
/// dedicated (large) carrier.
pub const CLASS_NONE: u16 = u16::MAX;

/// The largest number of size classes the header can encode.
///
/// Class indices are stored in 12 bits and the top encoding ([`CLASS_NONE`]'s
/// packed form) is the "no class" sentinel, so at most this many classes may be
/// registered. [`crate::size_class::SizeClasses::new`] enforces the limit.
pub const MAX_CLASS_COUNT: usize = 0xFFF;

/// The largest block size the header can encode (`size` is 32 bits of
/// [`UNIT_SIZE`]-sized units).
pub const MAX_BLOCK_SIZE: usize = (0xFFFF_FFFFu64 as usize).saturating_mul(UNIT_SIZE);

// Bit layout of the `bhdr` word (see the module docs).
const FLAGS_SHIFT: u32 = 0;
const CLASS_SHIFT: u32 = 8;
const OFFSET_SHIFT: u32 = CLASS_SHIFT + 12;
const SIZE_SHIFT: u32 = OFFSET_SHIFT + 12;

const CLASS_BITS: u64 = 0xFFF;
const OFFSET_BITS: u64 = 0xFFF;
const SIZE_BITS: u64 = 0xFFFF_FFFF;

// The whole header word must be fully accounted for by the field widths above.
const _: () = assert!(SIZE_SHIFT + 32 == 64);

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
/// `size`, `payload_offset` and `class` are recovered from the packed `bhdr` word;
/// `carrier` is the second header word (the owning carrier's base address); `flags`
/// is the raw flag byte (see [`BlockFlags`]).
#[derive(Clone, Copy, Debug)]
pub struct BlockHdr {
    /// Total block size in bytes, including the header and any alignment padding.
    pub size: usize,
    /// Byte offset from the block start to the payload.
    pub payload_offset: usize,
    /// Index into the allocator's [`crate::size_class::SizeClasses`], or
    /// [`CLASS_NONE`] for a dedicated (large) carrier block.
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
    /// `hdr.size` and `hdr.payload_offset` must be multiples of [`UNIT_SIZE`]
    /// (asserted in debug builds); `hdr.size` must not exceed [`MAX_BLOCK_SIZE`].
    ///
    /// # Safety
    ///
    /// `at` must point to at least [`BLOCK_HEADER_SIZE`] writable, `UNIT_SIZE`-aligned
    /// bytes owned by this allocator, with no live payload overlapping them.
    pub unsafe fn store(at: *mut u8, hdr: BlockHdr) {
        debug_assert_eq!(
            hdr.size % UNIT_SIZE,
            0,
            "block size must be UNIT_SIZE-granular"
        );
        debug_assert_eq!(
            hdr.payload_offset % UNIT_SIZE,
            0,
            "payload offset must be UNIT_SIZE-granular"
        );
        debug_assert!(
            hdr.size <= MAX_BLOCK_SIZE,
            "block size exceeds the encodable range"
        );
        let size_units = (hdr.size / UNIT_SIZE) as u64;
        let offset_units = (hdr.payload_offset / UNIT_SIZE) as u64;
        let class_bits = if hdr.class == CLASS_NONE {
            CLASS_BITS
        } else {
            u64::from(hdr.class) & CLASS_BITS
        };
        let bhdr = ((size_units & SIZE_BITS) << SIZE_SHIFT)
            | ((offset_units & OFFSET_BITS) << OFFSET_SHIFT)
            | (class_bits << CLASS_SHIFT)
            | (u64::from(hdr.flags) << FLAGS_SHIFT);
        // SAFETY: the caller guarantees `at` is `UNIT_SIZE`-aligned and writable for
        // at least `BLOCK_HEADER_SIZE` (= 2 words) bytes and that no live payload
        // overlaps the header, so the two word stores below are in-bounds and do not
        // clobber a live allocation.
        unsafe {
            at.cast::<u64>().write(bhdr);
            at.add(WORD_SIZE).cast::<*mut c_void>().write(hdr.carrier);
        }
    }

    /// Reads the block header at `at`.
    ///
    /// # Safety
    ///
    /// `at` must point to a block previously initialised by [`BlockHeader::store`] and
    /// still owned by this allocator.
    pub unsafe fn load(at: *mut u8) -> BlockHdr {
        // SAFETY: the caller guarantees `at` holds a header written by
        // `BlockHeader::store`: `UNIT_SIZE`-aligned, `BLOCK_HEADER_SIZE` readable
        // bytes, so both word loads below are in-bounds and aligned.
        let bhdr = unsafe { at.cast::<u64>().read() };
        // SAFETY: as above — the carrier word is the second header word.
        let carrier = unsafe { at.add(WORD_SIZE).cast::<*mut c_void>().read() };
        let size = (((bhdr >> SIZE_SHIFT) & SIZE_BITS) as usize).saturating_mul(UNIT_SIZE);
        let payload_offset =
            (((bhdr >> OFFSET_SHIFT) & OFFSET_BITS) as usize).saturating_mul(UNIT_SIZE);
        let class_bits = (bhdr >> CLASS_SHIFT) & CLASS_BITS;
        let class = if class_bits == CLASS_BITS {
            CLASS_NONE
        } else {
            class_bits as u16
        };
        // The flag byte occupies the low 8 bits of the header word by construction.
        let flags = ((bhdr >> FLAGS_SHIFT) & 0xFF) as u8;
        BlockHdr {
            size,
            payload_offset,
            class,
            carrier,
            flags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `UNIT_SIZE`-aligned scratch area standing in for a carrier.
    #[repr(align(16))]
    struct Scratch([u8; 256]);

    fn scratch() -> Box<Scratch> {
        Box::new(Scratch([0u8; 256]))
    }

    fn assert_same(loaded: BlockHdr, expected: BlockHdr) {
        assert_eq!(loaded.size, expected.size, "size");
        assert_eq!(
            loaded.payload_offset, expected.payload_offset,
            "payload_offset"
        );
        assert_eq!(loaded.class, expected.class, "class");
        assert_eq!(loaded.carrier, expected.carrier, "carrier");
        assert_eq!(loaded.flags, expected.flags, "flags");
    }

    #[test]
    fn header_geometry() {
        assert_eq!(BLOCK_HEADER_SIZE, UNIT_SIZE);
        assert_eq!(BLOCK_HEADER_WORDS, 2);
        assert_eq!(WORD_SIZE, 8);
        assert_eq!(BlockHeader::header_size(0), BLOCK_HEADER_SIZE);
        assert_eq!(BlockHeader::header_size(48), BLOCK_HEADER_SIZE + 48);
    }

    #[test]
    fn round_trips_a_slab_header() {
        let mut buf = scratch();
        let carrier = buf.0.as_mut_ptr().cast::<c_void>();
        let hdr = BlockHdr {
            size: 16 + 128, // header + a 128-byte payload (size class 128)
            payload_offset: 0,
            class: 3,
            carrier,
            flags: BlockFlags::empty().bits(),
        };
        // SAFETY: `buf` is 16-aligned and 256 bytes long, so it holds the 16-byte
        // header with room to spare, and nothing else aliases it.
        unsafe { BlockHeader::store(buf.0.as_mut_ptr(), hdr) };
        // SAFETY: `store` just initialised the header at that address.
        let loaded = unsafe { BlockHeader::load(buf.0.as_mut_ptr()) };
        assert_same(loaded, hdr);
    }

    #[test]
    fn round_trips_an_alignment_padded_header() {
        // A 64-byte-aligned request gets `64 - UNIT_SIZE` bytes of padding after the
        // fixed header, so its payload starts 64 bytes into the block.
        let mut buf = scratch();
        let carrier = 0xdead_beef_usize as *mut c_void;
        let hdr = BlockHdr {
            size: 16 + 48 + 4096,
            payload_offset: 48,
            class: CLASS_NONE,
            carrier,
            flags: {
                let mut flags = BlockFlags::empty();
                flags.set_large();
                flags.set_last_in_carrier();
                flags.set_free();
                flags.bits()
            },
        };
        // SAFETY: as above.
        unsafe { BlockHeader::store(buf.0.as_mut_ptr(), hdr) };
        // SAFETY: as above.
        let loaded = unsafe { BlockHeader::load(buf.0.as_mut_ptr()) };
        assert_same(loaded, hdr);
        assert_eq!(BlockHeader::header_size(loaded.payload_offset), 64);
    }

    #[test]
    fn flags_round_trip_bit_for_bit() {
        let mut buf = scratch();
        for bits in [0x00u8, 0x01, 0x02, 0x03, 0x04, 0x07, 0x80, 0xff] {
            let hdr = BlockHdr {
                size: 64,
                payload_offset: 0,
                class: 1,
                carrier: core::ptr::null_mut(),
                flags: bits,
            };
            // SAFETY: `buf` holds a header-sized, 16-aligned, unaliased area.
            unsafe { BlockHeader::store(buf.0.as_mut_ptr(), hdr) };
            // SAFETY: just stored.
            let loaded = unsafe { BlockHeader::load(buf.0.as_mut_ptr()) };
            assert_eq!(loaded.flags, bits);
        }
        assert_eq!(ALL_FLAGS, 0b111);
    }

    #[test]
    fn class_none_sentinel_round_trips() {
        let mut buf = scratch();
        let hdr = BlockHdr {
            size: 4096,
            payload_offset: 0,
            class: CLASS_NONE,
            carrier: core::ptr::null_mut(),
            flags: 0,
        };
        // SAFETY: header-sized, 16-aligned, unaliased scratch area.
        unsafe { BlockHeader::store(buf.0.as_mut_ptr(), hdr) };
        // SAFETY: just stored.
        let loaded = unsafe { BlockHeader::load(buf.0.as_mut_ptr()) };
        assert_eq!(loaded.class, CLASS_NONE);
        assert_eq!(loaded.size, 4096);
    }

    #[test]
    fn largest_addressable_block_size_is_encodable() {
        let mut buf = scratch();
        let hdr = BlockHdr {
            size: MAX_BLOCK_SIZE,
            payload_offset: 0,
            class: 0,
            carrier: core::ptr::null_mut(),
            flags: 0,
        };
        // SAFETY: header-sized, 16-aligned, unaliased scratch area.
        unsafe { BlockHeader::store(buf.0.as_mut_ptr(), hdr) };
        // SAFETY: just stored.
        let loaded = unsafe { BlockHeader::load(buf.0.as_mut_ptr()) };
        assert_eq!(loaded.size, MAX_BLOCK_SIZE);
    }

    #[test]
    fn flags_accessors() {
        let mut flags = BlockFlags::empty();
        assert!(!flags.is_free() && !flags.is_large() && !flags.last_in_carrier());
        flags.set_free();
        assert!(flags.is_free());
        flags.clear_free();
        assert!(!flags.is_free());
        flags.set_large();
        assert!(flags.is_large());
        flags.set_last_in_carrier();
        assert_eq!(flags.bits(), FLAG_LARGE | FLAG_LAST_IN_CARRIER);
        assert_eq!(BlockFlags::from_bits(flags.bits()), flags);
    }
}
