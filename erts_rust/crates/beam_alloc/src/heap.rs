//! The per-process bump heap: the production term-construction arena.
//!
//! `Heap` is the *production* counterpart of the minimal `beam_term::heap::Heap`
//! test arena. Its geometry is identical (`hp`/`hend`, fixed capacity) but it is
//! backed by a real [`AllocRef`], and it **never reallocates**: handed-out pointers
//! stay valid for the heap's lifetime, so growth happens by *replacement* — the GC
//! allocates a larger heap and copies into it — never by moving live terms
//! (workspace `CONTEXT.md` §2).
//!
//! Exhaustion is a **normal** condition: it is reported as
//! [`BeamError::Alloc`](beam_common::BeamError::Alloc) with
//! [`AllocKind::Heap`](beam_common::AllocKind) so the engine/GC layer can collect
//! and retry (see the crate `CONTEXT.md`, Design Decisions).
//!
//! `unsafe` is confined to this module's marked functions.

use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;

use beam_common::Result;
use beam_term::prelude::{
    make_list, make_tuple, try_make_arityval, Eterm, CAR_OFFSET, CDR_OFFSET,
};

use crate::allocator::{AllocRef, MIN_ALIGN};
use crate::error;
use crate::registry::default_allocator;

/// A fixed-capacity bump arena over a carrier-provided block.
pub struct Heap {
    alloc: AllocRef,
    base: *mut Eterm,
    cap: usize,
    used: usize,
    peak: usize,
}

impl Heap {
    /// A new heap of `capacity_words` term words, backed by the default allocator.
    pub fn new(capacity_words: usize) -> Result<Heap> {
        Heap::with_allocator(default_allocator().clone(), capacity_words)
    }

    /// A new heap of `capacity_words` term words, backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, capacity_words: usize) -> Result<Heap> {
        let bytes = capacity_words.saturating_mul(size_of::<Eterm>());
        let block = alloc.alloc_bytes(bytes)?;
        Ok(Heap {
            alloc,
            base: block.as_ptr().cast::<Eterm>(),
            cap: capacity_words,
            used: 0,
            peak: 0,
        })
    }

    /// Total capacity in term words.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Words handed out so far.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Words still available.
    pub fn available(&self) -> usize {
        self.cap - self.used
    }

    /// High-water mark of [`used`](Heap::used) since the last `reset`.
    pub fn peak_used(&self) -> usize {
        self.peak
    }

    /// Rewinds the bump pointer. This invalidates **all** previously handed-out
    /// pointers (the GC calls it only after copying live terms out).
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// The current allocation pointer (`hp`).
    pub fn hp(&self) -> *mut Eterm {
        // SAFETY: `base` is valid for `cap` words and `used <= cap`, so the
        // displacement is in-bounds (or exactly one-past-the-end when full).
        unsafe { self.base.add(self.used) }
    }

    /// One past the last word of the heap (`hend`).
    pub fn hend(&self) -> *mut Eterm {
        // SAFETY: `base` is valid for `cap` words, so `cap` is one-past-the-end.
        unsafe { self.base.add(self.cap) }
    }

    /// The allocator backing this heap.
    pub fn allocator(&self) -> &AllocRef {
        &self.alloc
    }

    /// Allocates `n` contiguous, **uninitialised** words.
    ///
    /// # Safety
    ///
    /// The returned words are uninitialised; the caller must write them before they
    /// are read, and must not use them after this heap is dropped. The heap never
    /// reallocates, so the pointer stays valid for the heap's lifetime.
    pub unsafe fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        if n > self.available() {
            return Err(error::heap_exhausted(n));
        }
        let p = self.hp();
        self.used += n;
        self.peak = self.peak.max(self.used);
        Ok(p)
    }

    /// Allocates a tuple of arity `n`: writes the arityval header and returns the
    /// header pointer, with `n` uninitialised element words following it.
    ///
    /// # Safety
    ///
    /// The `n` element words are uninitialised; the caller must write them before they
    /// are read, and must not use the pointer after this heap is dropped.
    pub unsafe fn alloc_tuple(&mut self, n: usize) -> Result<*mut Eterm> {
        let header = try_make_arityval(n)?;
        // SAFETY: `alloc_words` returned `n + 1` exclusively-owned, writable words.
        let p = unsafe { self.alloc_words(n + 1)? };
        // SAFETY: `p` points at the first of those words; the header goes there and
        // the remaining `n` words are left for the caller to fill.
        unsafe { *p = header };
        Ok(p)
    }

    /// Builds a cons cell `[car, cdr]` and returns the list term.
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm> {
        // SAFETY: `alloc_words` returned two exclusively-owned, writable words.
        let p = unsafe { self.alloc_words(2)? };
        // SAFETY: `p` is the base of the two words just allocated.
        unsafe {
            *p.add(CAR_OFFSET) = car;
            *p.add(CDR_OFFSET) = cdr;
        }
        Ok(make_list(p as *const Eterm))
    }

    /// Builds a tuple from `elems` and returns the tuple term.
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        // SAFETY: `alloc_tuple` writes the header and leaves `elems.len()` writable
        // element words.
        let p = unsafe { self.alloc_tuple(elems.len())? };
        for (i, elem) in elems.iter().enumerate() {
            // SAFETY: `p` owns `1 + elems.len()` words, so `1 + i` is in-bounds.
            unsafe { *p.add(1 + i) = *elem };
        }
        Ok(make_tuple(p))
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        let bytes = self.cap * size_of::<Eterm>();
        let layout = match Layout::from_size_align(bytes, MIN_ALIGN) {
            Ok(layout) => layout,
            // `with_allocator` built this layout, so it must be valid; there is
            // nothing sensible to do in `Drop` if it somehow is not.
            Err(_) => return,
        };
        let ptr = match NonNull::new(self.base.cast::<u8>()) {
            Some(ptr) => ptr,
            // A null base means nothing was handed out (zero-capacity heap).
            None => return,
        };
        // SAFETY: `base` came from `self.alloc.alloc_bytes(bytes)` in
        // `with_allocator` with this exact size and alignment, and is freed exactly
        // once here (the heap never reallocates).
        unsafe { self.alloc.dealloc(ptr, layout) };
    }
}
