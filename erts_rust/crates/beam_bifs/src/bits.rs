//! Heap bitstring/binary helpers shared by the binary and unicode BIFs.
//!
//! This module owns the *safe* API for building and reading **on-heap
//! bitstrings** (`ErlHeapBits`) — the frozen `[thing_word, size, data…]` block
//! (`beam_term::boxed::ERL_HEAP_BITS_SIZE`, `../../erts/emulator/beam/erl_bits.h`
//! `header_heap_bits`). The raw word access lives in [`crate::access`] (the crate's
//! only `#[allow(unsafe_code)]` module); this module layers the byte/bit-level
//! semantics on top and is otherwise safe Rust.
//!
//! Interior layout (owned by `beam_term`, reproduced here):
//! `word 0 = make_header(1 + data_words, HEAP_BITS_SUBTAG)`, `word 1 = bit size`,
//! `words 2.. = the bit string`, packed MSB-first starting at the low byte of
//! `word 2`. For a byte-aligned binary the bytes are stored in order, so a binary
//! of `n` bytes occupies `2 + ceil(n / 8)` words.
