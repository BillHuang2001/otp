//! The `enif_*` host functions — the Rust side of the NIF C ABI (workspace contract **A.3**).
//!
//! Every function here is a real `#[no_mangle] pub unsafe extern "C" fn` whose signature matches the
//! corresponding field of the frozen [`super::api::ErlNifApi`] table **exactly**. Each one drives the
//! per-call [`NifEnv`]/[`super::host::NifHost`] seam in [`super::host`]: term construction goes
//! through `beam_term` writers over the host's [`beam_alloc::Heap`], atoms through
//! `NifHost::intern_atom`.
//!
//! [`enif_api_table`] exposes the `#[used] static ENIF_API` — the runtime's `enif_*` surface as a
//! populated table. The `#[used]` static is what keeps the `#[no_mangle]` symbols in the linkage of a
//! binary that pulls this crate in (a loaded NIF `.so` resolves them through `--export-dynamic`).
//!
//! # Functions that stay `None` — and why
//!
//! Stable Rust cannot give a body to a **C-variadic** function (a `...` in a `fn` *definition* is a
//! nightly-only feature). These six table slots are therefore left `None` and are the *only* reason
//! the table is not fully populated:
//!
//! | slot | same need covered by |
//! |---|---|
//! | `enif_make_tuple` | [`enif_make_tuple_from_array`] |
//! | `enif_make_list` | [`enif_make_list_from_array`] |
//! | `enif_set_option` | (no non-variadic equivalent; unused on this runtime) |
//! | `enif_fprintf` / `enif_snprintf` / `enif_vfprintf` / `enif_vsnprintf` | (formatting helpers) |
//!
//! Every other slot that is `None` is **out of this step's scope** (resources, `ioq`, threads/mutex/
//! cond/rwlock/tsd, select, monitors, `dlopen`/`dlsym`, time, `enif_send`/`enif_port_command`,
//! `enif_schedule_nif`, `whereis_*`, `system_info`, `getenv`, `consume_timeslice`, `is_*_alive`,
//! `thread_type`, the resource and atom-cache APIs, `enif_alloc_env`) — the runtime subsystems that
//! own them (`beam_rt`/`beam_bifs`/`beam_io`) have not been wired to this crate yet.
//!
//! # `ErlNifBinary` ownership marker
//!
//! The C runtime distinguishes an *owned* (`enif_alloc_binary`'d) `ErlNifBinary` from an *inspected*
//! zero-copy view through `ref_bin` (`NULL` = owned). Here `ref_bin` is always `NULL` and the
//! distinction lives in `__spare__[0]`:
//!
//! * `__spare__[0] == null` — **owned**: the payload was `malloc`'d and [`enif_release_binary`] /
//!   [`enif_realloc_binary`] may `free`/`realloc` it.
//! * `__spare__[0] != null` (the sentinel `1 as *mut c_void`) — **inspected**: the payload points
//!   into the BEAM heap; release is a no-op.
//!
//! # Honest limitations (never a wrong result)
//!
//! * **Integers** are handled for small ints and for bignums up to one 64-bit limb; anything wider
//!   fails the accessor (returns `0`) instead of truncating.
//! * **Atom comparison / hashing** ([`enif_compare`], [`enif_is_identical`], [`enif_hash`]) has no
//!   env parameter, hence no host, hence no atom-name table: identical terms are handled by a raw-word
//!   fast path, and distinct atoms fall back to `beam_term`'s index fallback.
//! * **Map operations** use an empty `AtomTable` for the same reason; they are exact whenever the
//!   key terms are not atoms (the overwhelmingly common NIF case, where the key is a term the NIF was
//!   handed).
//! * **Map iteration** is not implemented (hash-map iteration needs a dynamic work stack).
//! * [`enif_make_binary`] builds an **on-heap** binary (off-heap refcounted binaries are a `beam_alloc`
//!   concern not yet exposed here); [`enif_inspect_binary`] refuses a *bit*-offset base instead of
//!   fabricating an unaligned pointer.

use libc::{c_char, c_double, c_int, c_long, c_uchar, c_uint, c_ulong, c_void, size_t};

use beam_term::atom::AtomTable;
use beam_term::prelude::{self, Eterm, NIL};

use crate::nif::api::{ErlNifApi, ERL_NIF_API_FIELD_COUNT};
use crate::nif::host::NifEnv;
use crate::nif::term::{eterm_from_nif, nif_from_eterm, ERL_NIF_TERM};
use crate::nif::types::{
    ErlNifBinary, ErlNifCharEncoding, ErlNifHash, ErlNifPid, ErlNifPort, ErlNifTermType,
};

// The table this module populates must keep its frozen shape.
const _: () = assert!(ERL_NIF_API_FIELD_COUNT == 176);

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Recover the live [`NifEnv`] behind an opaque pointer (`None` when null).
///
/// # Safety
///
/// `env` must be null, or a pointer previously produced by `NifEnv::as_ptr` for an env that is still
/// alive (with its host still alive).
#[inline]
unsafe fn env_mut<'a>(env: *mut crate::nif::types::ErlNifEnv) -> Option<&'a mut NifEnv> {
    // SAFETY: delegated to `NifEnv::from_opaque`, which checks for null and documents the same
    // contract (pointer from `as_ptr` of a live env, whose host outlives the borrow).
    unsafe { NifEnv::from_opaque(env) }
}

/// A borrowed `&str` view over a caller-provided C string (`None` if the pointer is null).
///
/// # Safety
///
/// `ptr` must be null or point at a NUL-terminated byte sequence valid for the call.
#[inline]
unsafe fn cstr<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string; `CStr::from_ptr` only reads up to it.
    Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes())
}

/// A borrowed view over `len` bytes at `ptr` (`None` if null and `len > 0`).
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes (a null `ptr` is accepted only for `len == 0`).
#[inline]
unsafe fn cbuf<'a>(ptr: *const c_char, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return if len == 0 { Some(&[]) } else { None };
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

/// NIF term → internal term.
#[inline]
fn t(term: ERL_NIF_TERM) -> Eterm {
    eterm_from_nif(term)
}

/// Internal term → NIF term.
#[inline]
fn n(term: Eterm) -> ERL_NIF_TERM {
    nif_from_eterm(term)
}

/// A single-value term-out write: `true` when `ptr` was non-null and received `value`.
///
/// # Safety
///
/// `ptr` must be null or valid for one write of `T`.
#[inline]
unsafe fn write_out<T>(ptr: *mut T, value: T) -> bool {
    if ptr.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees `ptr` is valid for one write; null was handled above.
    unsafe { ptr.write(value) };
    true
}

/// Allocate `words` uninitialized heap words from the env's host heap (`None` on exhaustion).
///
/// # Safety
///
/// The returned pointer is only valid while the env (and hence the host heap) is alive.
#[inline]
unsafe fn alloc(env: &mut NifEnv, words: usize) -> Option<*mut Eterm> {
    // SAFETY: a plain bump allocation of `words` words; exhaustion is reported as `None`.
    unsafe { env.heap().alloc_words(words) }.ok()
}

/// The `badarg` atom for `env`, or `NIL` when the host cannot intern atoms.
fn badarg_atom(env: &mut NifEnv) -> Eterm {
    env.host().intern_atom("badarg").unwrap_or(NIL)
}

/// `badarg` as a NIF term without raising (used where no env is available).
const fn bare_badarg() -> ERL_NIF_TERM {
    ERL_NIF_TERM::from_raw(NIL.raw())
}

/// The `undefined` atom term used for an "undefined" `ErlNifPid` — `NIL` (`[]`), so an undefined pid
/// is a value that can never be a live internal pid term.
const UNDEFINED_PID: ERL_NIF_TERM = ERL_NIF_TERM::from_raw(NIL.raw());

/// Whether `t` is a pid (internal or external).
#[inline]
fn is_pid(t: Eterm) -> bool {
    if !is_term_word(t) {
        return false;
    }
    prelude::is_internal_pid(t)
        // SAFETY: `is_external_pid` only dereferences the header of a boxed term.
        || unsafe { prelude::is_external_pid(t) }
}

/// Whether `t` is a port (internal or external).
#[inline]
fn is_port(t: Eterm) -> bool {
    if !is_term_word(t) {
        return false;
    }
    prelude::is_internal_port(t)
        // SAFETY: as `is_pid`.
        || unsafe { prelude::is_external_port(t) }
}

/// Whether `t` is a reference (internal or external).
#[inline]
fn is_ref(t: Eterm) -> bool {
    if !is_term_word(t) {
        return false;
    }
    // SAFETY: the predicates only dereference the header of a boxed term.
    unsafe { prelude::is_internal_ref(t) || prelude::is_external_ref(t) }
}

/// A fresh (empty) atom table. Used by the map/compare API, which has no env (see the module docs).
fn no_atoms() -> AtomTable {
    AtomTable::new()
}

/// Whether `term` is a *term word at all* — i.e. it is not the invalid `HEADER` pattern.
///
/// A term carries one of the primary tags `LIST`, `BOXED` or `IMMED1`; the `HEADER` pattern
/// (primary tag `0` — the raw word `0`, `THE_NON_VALUE`, or any bare header word) is **not** a term.
/// The C's `is_boxed` bit trick classifies it as *boxed* (`(x & (MASK-BOXED)) == 0` holds for `0`),
/// so `is_binary`/`is_tuple`/`size_object`/… would dereference a null pointer on it — in the C that
/// is a `SIGSEGV`. Every `enif_*` entry point below therefore rejects such a word up front and
/// returns the C-safe default (`0`, `NULL`, `None`), so an errored or hostile NIF cannot take the
/// runtime down. This is strictly *safer* than the C and never changes the answer for a valid term.
#[inline]
fn is_term_word(term: Eterm) -> bool {
    (term.raw() & prelude::TAG_PRIMARY_MASK) != prelude::TAG_PRIMARY_HEADER
}

// ---------------------------------------------------------------------------------------------
// Integer conversion (C `term_to_Sint` / `term_to_Uint`, big.c)
// ---------------------------------------------------------------------------------------------

/// `term_to_Sint` — the term's value as an `i64`, or `None` when it is not an integer that fits.
fn term_to_sint64(t: Eterm) -> Option<i64> {
    if !is_term_word(t) {
        return None;
    }
    if prelude::is_small(t) {
        return Some(prelude::signed_value(t));
    }
    // SAFETY: `is_big` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_big(t) } {
        return None;
    }
    // SAFETY: `t` is a live bignum (just tested), so `big_parts` is in contract.
    let (negative, digits) = unsafe { prelude::big_parts(t) };
    // `xl * D_EXP > 64` rejects anything wider than one 64-bit limb (big.c:3666).
    if digits.len() != 1 {
        return None;
    }
    let magnitude = digits[0];
    if negative {
        // `-2^63` is representable; a larger magnitude is not.
        if magnitude > (1u64 << 63) {
            return None;
        }
        Some((magnitude as i64).wrapping_neg())
    } else {
        if magnitude > i64::MAX as u64 {
            return None;
        }
        Some(magnitude as i64)
    }
}

/// `term_to_Uint` — the term's value as a `u64`, or `None` (negative, or wider than 64 bits).
fn term_to_uint64(t: Eterm) -> Option<u64> {
    if !is_term_word(t) {
        return None;
    }
    if prelude::is_small(t) {
        let v = prelude::signed_value(t);
        return if v < 0 { None } else { Some(v as u64) };
    }
    // SAFETY: `is_big` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_big(t) } {
        return None;
    }
    // SAFETY: `t` is a live bignum (just tested).
    let (negative, digits) = unsafe { prelude::big_parts(t) };
    if negative || digits.len() != 1 {
        return None;
    }
    Some(digits[0])
}

// ---------------------------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------------------------

/// `void *enif_alloc(size_t size);`
///
/// # Safety
///
/// The returned block must be released with [`enif_free`] (or [`enif_realloc`]); no provenance
/// obligations beyond that of `malloc`.
#[no_mangle]
pub unsafe extern "C" fn enif_alloc(size: size_t) -> *mut c_void {
    // SAFETY: `malloc` returns null on failure, which the caller must check (C semantics).
    unsafe { libc::malloc(size) }
}

/// `void enif_free(void *ptr);`
///
/// # Safety
///
/// `ptr` must be null or a pointer returned by [`enif_alloc`]/[`enif_realloc`] that has not already
/// been freed.
#[no_mangle]
pub unsafe extern "C" fn enif_free(ptr: *mut c_void) {
    // SAFETY: `free(NULL)` is a no-op; the caller guarantees any non-null pointer was `malloc`'d.
    unsafe { libc::free(ptr) };
}

/// `void *enif_realloc(void *ptr, size_t size);`
///
/// # Safety
///
/// `ptr` must be null or a pointer returned by [`enif_alloc`]/[`enif_realloc`] that has not already
/// been freed.
#[no_mangle]
pub unsafe extern "C" fn enif_realloc(ptr: *mut c_void, size: size_t) -> *mut c_void {
    // SAFETY: `realloc(NULL, n)` == `malloc(n)`; the caller guarantees the block's provenance.
    unsafe { libc::realloc(ptr, size) }
}

// ---------------------------------------------------------------------------------------------
// Binaries
// ---------------------------------------------------------------------------------------------

/// The sentinel stored in `__spare__[0]` for an *inspected* (non-owned) binary view.
const INSPECTED: *mut c_void = 1 as *mut c_void;

/// `int enif_alloc_binary(size_t size, ErlNifBinary* bin);`
///
/// # Safety
///
/// `bin` must be null or point at a writable `ErlNifBinary`.
#[no_mangle]
pub unsafe extern "C" fn enif_alloc_binary(size: size_t, bin: *mut ErlNifBinary) -> c_int {
    if bin.is_null() {
        return 0;
    }
    // SAFETY: `malloc(size)` yields a `size`-byte writable block (or null).
    let data = unsafe { libc::malloc(size) }.cast::<c_uchar>();
    if data.is_null() {
        return 0;
    }
    // SAFETY: `bin` is non-null and writable (caller contract); an owned buffer's marker is null.
    unsafe {
        bin.write(ErlNifBinary {
            size,
            data,
            ref_bin: core::ptr::null_mut(),
            __spare__: [core::ptr::null_mut(), core::ptr::null_mut()],
        });
    }
    1
}

/// `int enif_realloc_binary(ErlNifBinary* bin, size_t size);`
///
/// # Safety
///
/// `bin` must be null or point at a live `ErlNifBinary` produced by this module.
#[no_mangle]
pub unsafe extern "C" fn enif_realloc_binary(bin: *mut ErlNifBinary, size: size_t) -> c_int {
    if bin.is_null() {
        return 0;
    }
    // SAFETY: `bin` is non-null (checked) and live (caller contract).
    let b = unsafe { &mut *bin };
    if b.__spare__[0].is_null() {
        // Owned: a plain realloc keeps the contents.
        // SAFETY: the block came from `malloc`/`realloc` and is owned by this struct.
        let grown = unsafe { libc::realloc(b.data.cast::<c_void>(), size) }.cast::<c_uchar>();
        if grown.is_null() {
            return 0;
        }
        b.data = grown;
        b.size = size;
        return 1;
    }
    // Inspected: allocate a fresh owned buffer and copy what fits.
    // SAFETY: a fresh `size`-byte block.
    let fresh = unsafe { libc::malloc(size) }.cast::<c_uchar>();
    if fresh.is_null() {
        return 0;
    }
    let copy = core::cmp::min(size, b.size);
    if copy > 0 && !b.data.is_null() {
        // SAFETY: both buffers are valid for `copy` bytes (`copy <= size` and `copy <= b.size`).
        unsafe { core::ptr::copy_nonoverlapping(b.data, fresh, copy) };
    }
    b.data = fresh;
    b.size = size;
    b.__spare__[0] = core::ptr::null_mut();
    1
}

/// `void enif_release_binary(ErlNifBinary* bin);`
///
/// # Safety
///
/// `bin` must be null or point at a live `ErlNifBinary` produced by this module.
#[no_mangle]
pub unsafe extern "C" fn enif_release_binary(bin: *mut ErlNifBinary) {
    if bin.is_null() {
        return;
    }
    // SAFETY: `bin` is non-null (checked) and live (caller contract).
    let b = unsafe { &mut *bin };
    if b.__spare__[0].is_null() && !b.data.is_null() {
        // SAFETY: an owned (non-inspected) payload was `malloc`'d here; free it exactly once.
        unsafe { libc::free(b.data.cast::<c_void>()) };
    }
    b.data = core::ptr::null_mut();
    b.size = 0;
    b.__spare__[0] = core::ptr::null_mut();
}

/// `int enif_inspect_binary(ErlNifEnv*, ERL_NIF_TERM bin_term, ErlNifBinary* bin);`
///
/// # Safety
///
/// `bin` must be null or writable; `bin_term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_inspect_binary(
    _env: *mut crate::nif::types::ErlNifEnv,
    bin_term: ERL_NIF_TERM,
    bin: *mut ErlNifBinary,
) -> c_int {
    if bin.is_null() {
        return 0;
    }
    let term = t(bin_term);
    if !is_term_word(term) {
        return 0;
    }
    // SAFETY: `is_bitstring` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_bitstring(term) } {
        return 0;
    }
    // SAFETY: `term` is a live bitstring (just tested).
    let bits = unsafe { prelude::bitstring_size(term) };
    if bits & 7 != 0 {
        return 0;
    }
    // SAFETY: as above; `bitstring_parts` reports `(base, bit_offset, bit_size)`.
    let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
    if offset & 7 != 0 || base.is_null() {
        // An unaligned base cannot be exposed as a byte buffer without a copy; refuse honestly.
        return 0;
    }
    // SAFETY: the payload starts at `base + offset/8` and is `bits/8` bytes long.
    let payload = unsafe { base.add(offset / 8) }.cast_mut();
    // SAFETY: `bin` is non-null and writable; `payload` borrows the live term's storage.
    unsafe {
        bin.write(ErlNifBinary {
            size: bits / 8,
            data: payload,
            ref_bin: core::ptr::null_mut(),
            __spare__: [INSPECTED as *mut c_void, core::ptr::null_mut()],
        });
    }
    1
}

/// Append the bytes of an iolist (or a binary) to `out`; `false` for anything else.
fn flatten_iolist(term: Eterm, out: &mut Vec<u8>) -> bool {
    if !is_term_word(term) {
        return false;
    }
    if prelude::is_nil(term) {
        return true;
    }
    // SAFETY: `is_bitstring` only dereferences the header of a boxed term.
    if unsafe { prelude::is_binary(term) } {
        // SAFETY: `term` is a live byte-aligned bitstring.
        let bits = unsafe { prelude::bitstring_size(term) };
        // SAFETY: as above.
        let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
        if offset & 7 != 0 || base.is_null() {
            return false;
        }
        // SAFETY: the byte payload base+offset/8 is `bits/8` bytes long.
        let bytes = unsafe { core::slice::from_raw_parts(base.add(offset / 8), bits / 8) };
        out.extend_from_slice(bytes);
        return true;
    }
    if prelude::is_list(term) {
        // SAFETY: `term` is a live cons cell.
        let (head, tail) = unsafe { (prelude::car(term), prelude::cdr(term)) };
        return flatten_iolist(head, out) && flatten_iolist(tail, out);
    }
    if prelude::is_small(term) {
        let v = prelude::signed_value(term);
        if (0..=255).contains(&v) {
            out.push(v as u8);
            return true;
        }
    }
    false
}

/// `int enif_inspect_iolist_as_binary(ErlNifEnv*, ERL_NIF_TERM term, ErlNifBinary* bin);`
///
/// A flat binary yields the same zero-copy view as [`enif_inspect_binary`]; a list is flattened into
/// a freshly `malloc`'d (owned) buffer the caller must release.
///
/// # Safety
///
/// `bin` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_inspect_iolist_as_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    bin: *mut ErlNifBinary,
) -> c_int {
    // SAFETY: forwards the caller's pointer contract unchanged.
    if unsafe { enif_inspect_binary(env, term, bin) } != 0 {
        return 1;
    }
    if bin.is_null() {
        return 0;
    }
    let mut bytes = Vec::new();
    if !flatten_iolist(t(term), &mut bytes) {
        return 0;
    }
    let len = bytes.len();
    // SAFETY: `bytes` is a live slice of `len` bytes.
    // SAFETY: a fresh writable `len`-byte block.
    let data = if len == 0 {
        core::ptr::null_mut()
    } else {
        let p = unsafe { libc::malloc(len) }.cast::<c_uchar>();
        if p.is_null() {
            return 0;
        }
        // SAFETY: `p` is `len` writable bytes and `bytes` has `len` readable bytes (disjoint).
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, len) };
        p
    };
    // SAFETY: `bin` is non-null and writable; the buffer is owned (marker null).
    unsafe {
        bin.write(ErlNifBinary {
            size: len,
            data,
            ref_bin: core::ptr::null_mut(),
            __spare__: [core::ptr::null_mut(), core::ptr::null_mut()],
        });
    }
    1
}

/// `unsigned char* enif_make_new_binary(ErlNifEnv*, size_t size, ERL_NIF_TERM* termp);`
///
/// Reserves an on-heap binary of `size` bytes, writes the term through `termp`, and returns a
/// writable pointer to its payload (C semantics).
///
/// # Safety
///
/// `env` must be a live env; `termp` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_new_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    size: size_t,
    termp: *mut ERL_NIF_TERM,
) -> *mut c_uchar {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return core::ptr::null_mut();
    };
    let n_bits = size * 8;
    // SAFETY: `heap_bits_size_words` sizes the `[hdr][size][data…]` block.
    let Some(hp) = (unsafe { alloc(e, prelude::heap_bits_size_words(n_bits)) }) else {
        return core::ptr::null_mut();
    };
    let zeros = vec![0u8; size];
    // SAFETY: `hp` heads a block of `heap_bits_size_words(n_bits)` words and `zeros` holds exactly
    // the `size` payload bytes the bit count needs.
    let Ok(term) = (unsafe { prelude::write_heap_bits(hp, &zeros, n_bits) }) else {
        return core::ptr::null_mut();
    };
    if !(unsafe { write_out(termp, n(term)) }) {
        return core::ptr::null_mut();
    }
    // SAFETY: `term` is the heap binary just written, so its payload pointer is live and writable.
    unsafe { prelude::bitstring_parts(term).0.cast_mut().cast::<c_uchar>() }
}

/// `ERL_NIF_TERM enif_make_binary(ErlNifEnv* env, ErlNifBinary* bin);`
///
/// Builds an **on-heap** binary copying `bin`'s bytes, then consumes `bin` (releases an owned buffer
/// and zeroes the struct) exactly as the C does.
///
/// # Safety
///
/// `env` must be a live env; `bin` must be a live `ErlNifBinary`.
#[no_mangle]
pub unsafe extern "C" fn enif_make_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    bin: *mut ErlNifBinary,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    if bin.is_null() {
        return bare_badarg();
    }
    // SAFETY: `bin` is non-null (checked) and live (caller contract).
    let b = unsafe { &mut *bin };
    let bytes: &[u8] = if b.data.is_null() || b.size == 0 {
        &[]
    } else {
        // SAFETY: the struct declares `size` readable bytes at `data`.
        unsafe { core::slice::from_raw_parts(b.data, b.size) }
    };
    // SAFETY: `heap_bits_size_words` sizes the block for `size * 8` bits.
    let Some(hp) = (unsafe { alloc(e, prelude::heap_bits_size_words(b.size * 8)) }) else {
        return bare_badarg();
    };
    // SAFETY: `hp` heads the reserved block and `bytes` provides the payload.
    let term = match unsafe { prelude::write_binary(hp, bytes) } {
        Ok(term) => term,
        Err(_) => return bare_badarg(),
    };
    // Consume the caller's binary (C: `enif_release_binary(bin)`).
    // SAFETY: `bin` is live and owned/inspected per its own marker.
    unsafe { enif_release_binary(bin) };
    n(term)
}

/// `ERL_NIF_TERM enif_make_sub_binary(ErlNifEnv*, ERL_NIF_TERM bin_term, size_t pos, size_t size);`
///
/// # Safety
///
/// `env` must be a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_sub_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    bin_term: ERL_NIF_TERM,
    pos: size_t,
    size: size_t,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let term = t(bin_term);
    if !is_term_word(term) {
        return n(badarg_atom(e));
    }
    // SAFETY: `is_binary` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_binary(term) } {
        return n(badarg_atom(e));
    }
    // SAFETY: `term` is a live byte-aligned bitstring.
    let bits = unsafe { prelude::bitstring_size(term) };
    let total_bytes = bits / 8;
    if pos > total_bytes || size > total_bytes - pos {
        return n(badarg_atom(e));
    }
    if pos == 0 && size == total_bytes {
        return bin_term;
    }
    // SAFETY: as above — `(base, bit_offset, _)`.
    let (base, offset, _) = unsafe { prelude::bitstring_parts(term) };
    if offset & 7 != 0 || base.is_null() {
        return n(badarg_atom(e));
    }
    // SAFETY: `sub_bits_size_words()` sizes the 5-word `ErlSubBits` block.
    let Some(hp) = (unsafe { alloc(e, prelude::sub_bits_size_words()) }) else {
        return bare_badarg();
    };
    let start = (pos * 8) as usize;
    let end = ((pos + size) * 8) as usize;
    // SAFETY: `hp` heads the reserved 5-word block; the base pointer identifies the original
    // payload (kept alive by `term`'s heap), and the window is inside it by the checks above.
    let sub = unsafe {
        prelude::write_sub_bits(
            hp,
            Eterm::from_raw((base as usize) & !prelude::ERL_SUB_BITS_FLAG_MASK),
            Eterm::from_raw(start),
            Eterm::from_raw(end),
            term,
        )
    };
    n(sub)
}

/// `int enif_term_to_binary(ErlNifEnv *env, ERL_NIF_TERM term, ErlNifBinary *bin);`
///
/// # Safety
///
/// `env` must be a live env; `bin` must be writable.
#[no_mangle]
pub unsafe extern "C" fn enif_term_to_binary(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    bin: *mut ErlNifBinary,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if bin.is_null() {
        return 0;
    }
    if !is_term_word(t(term)) {
        return 0;
    }
    let Some(bytes) = e.host().term_to_binary(t(term)) else {
        return 0;
    };
    let len = bytes.len();
    // SAFETY: a fresh `len`-byte writable block (owned).
    let data = if len == 0 {
        core::ptr::null_mut()
    } else {
        let p = unsafe { libc::malloc(len) }.cast::<c_uchar>();
        if p.is_null() {
            return 0;
        }
        // SAFETY: `p` is `len` writable bytes and `bytes` `len` readable bytes (disjoint).
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, len) };
        p
    };
    // SAFETY: `bin` is non-null and writable; the buffer is owned.
    unsafe {
        bin.write(ErlNifBinary {
            size: len,
            data,
            ref_bin: core::ptr::null_mut(),
            __spare__: [core::ptr::null_mut(), core::ptr::null_mut()],
        });
    }
    1
}

/// `size_t enif_binary_to_term(ErlNifEnv*, const unsigned char* data, size_t sz, ERL_NIF_TERM* term, unsigned int opts);`
///
/// Returns the number of bytes consumed, or `0` on failure.
///
/// # Safety
///
/// `env` must be a live env; `data` must be `sz` readable bytes; `term` must be writable.
#[no_mangle]
pub unsafe extern "C" fn enif_binary_to_term(
    env: *mut crate::nif::types::ErlNifEnv,
    data: *const c_uchar,
    sz: size_t,
    term: *mut ERL_NIF_TERM,
    _opts: c_uint,
) -> size_t {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if data.is_null() || term.is_null() {
        return 0;
    }
    // SAFETY: the caller declares `sz` readable bytes at `data`.
    let bytes = unsafe { core::slice::from_raw_parts(data, sz) };
    match e.host().binary_to_term(bytes) {
        Some(value) => {
            // SAFETY: `term` is non-null and writable (checked above).
            unsafe { term.write(n(value)) };
            sz
        }
        None => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------------------------

/// `int enif_get_int(ErlNifEnv*, ERL_NIF_TERM term, int* ip);`
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_int(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut c_int,
) -> c_int {
    match term_to_sint64(t(term)) {
        Some(v) if (c_int::MIN as i64..=c_int::MAX as i64).contains(&v) => {
            // SAFETY: `ip` may be null (handled) or valid for one write (caller contract).
            (unsafe { write_out(ip, v as c_int) }) as c_int
        }
        _ => 0,
    }
}

/// `int enif_get_uint(ErlNifEnv*, ERL_NIF_TERM term, unsigned* ip);`
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_uint(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut c_uint,
) -> c_int {
    match term_to_uint64(t(term)) {
        Some(v) if v <= c_uint::MAX as u64 => {
            // SAFETY: `ip` may be null (handled) or valid for one write.
            (unsafe { write_out(ip, v as c_uint) }) as c_int
        }
        _ => 0,
    }
}

/// `int enif_get_long(ErlNifEnv*, ERL_NIF_TERM term, long* ip);`
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_long(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut c_long,
) -> c_int {
    match term_to_sint64(t(term)) {
        // SAFETY: `ip` may be null (handled) or valid for one write.
        Some(v) => (unsafe { write_out(ip, v as c_long) }) as c_int,
        None => 0,
    }
}

/// `int enif_get_ulong(ErlNifEnv*, ERL_NIF_TERM term, unsigned long* ip);`
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_ulong(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut c_ulong,
) -> c_int {
    match term_to_uint64(t(term)) {
        // SAFETY: `ip` may be null (handled) or valid for one write.
        Some(v) => (unsafe { write_out(ip, v as c_ulong) }) as c_int,
        None => 0,
    }
}

/// `int enif_get_int64(ErlNifEnv*, ERL_NIF_TERM term, ErlNifSInt64* ip);`
///
/// On LP64 the header `#define`s this onto [`enif_get_long`]; it is exported here as a free symbol
/// too (it is not a separate `ErlNifApi` slot on LP64).
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_int64(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut i64,
) -> c_int {
    // SAFETY: `ip` is a `*mut i64` and `c_long` is `i64` on LP64; same contract.
    unsafe { enif_get_long(env, term, ip.cast::<c_long>()) }
}

/// `int enif_get_uint64(ErlNifEnv*, ERL_NIF_TERM term, ErlNifUInt64* ip);`
///
/// On LP64 the header `#define`s this onto [`enif_get_ulong`].
///
/// # Safety
///
/// `ip` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_uint64(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    ip: *mut u64,
) -> c_int {
    // SAFETY: `ip` is a `*mut u64` and `c_ulong` is `u64` on LP64; same contract.
    unsafe { enif_get_ulong(env, term, ip.cast::<c_ulong>()) }
}

/// `int enif_get_double(ErlNifEnv*, ERL_NIF_TERM term, double* dp);`
///
/// # Safety
///
/// `dp` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_double(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    dp: *mut c_double,
) -> c_int {
    let term = t(term);
    if !is_term_word(term) {
        return 0;
    }
    // SAFETY: `is_float` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_float(term) } {
        return 0;
    }
    // SAFETY: `term` is a live boxed float.
    let value = unsafe { prelude::float_value(term) };
    // SAFETY: `dp` may be null (handled) or valid for one write.
    (unsafe { write_out(dp, value) }) as c_int
}

/// `int enif_get_atom(ErlNifEnv*, ERL_NIF_TERM atom, char* buf, unsigned len, ErlNifCharEncoding);`
///
/// Returns the number of bytes written **including the NUL**, or `0` on failure.
///
/// # Safety
///
/// `env` must be a live env; `buf` must be null or point at `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_get_atom(
    env: *mut crate::nif::types::ErlNifEnv,
    atom: ERL_NIF_TERM,
    buf: *mut c_char,
    len: c_uint,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let name = match e.host().atom_name(t(atom)) {
        Some(name) => name,
        None => return 0,
    };
    let len = len as usize;
    if len == 0 || buf.is_null() {
        return 0;
    }
    match encoding {
        ErlNifCharEncoding::Latin1 => {
            let mut bytes = Vec::with_capacity(name.len());
            for ch in name.chars() {
                let code = ch as u32;
                if code > 0xFF {
                    // Not representable in Latin-1 (the C's `latin1_chars < 0`).
                    return 0;
                }
                bytes.push(code as u8);
            }
            if bytes.len() >= len {
                return 0;
            }
            // SAFETY: `buf` has `len > bytes.len()` writable bytes; write the name and its NUL.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), buf, bytes.len());
                buf.add(bytes.len()).write(0);
            }
            (bytes.len() + 1) as c_int
        }
        ErlNifCharEncoding::Utf8 => {
            let bytes = name.as_bytes();
            if bytes.len() >= len {
                return 0;
            }
            // SAFETY: `buf` has `len > bytes.len()` writable bytes; write the name and its NUL.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), buf, bytes.len());
                buf.add(bytes.len()).write(0);
            }
            (bytes.len() + 1) as c_int
        }
    }
}

/// `int enif_get_atom_length(ErlNifEnv*, ERL_NIF_TERM atom, unsigned* len, ErlNifCharEncoding);`
///
/// # Safety
///
/// `env` must be a live env; `len` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_atom_length(
    env: *mut crate::nif::types::ErlNifEnv,
    atom: ERL_NIF_TERM,
    len: *mut c_uint,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let Some(name) = e.host().atom_name(t(atom)) else {
        return 0;
    };
    let n = match encoding {
        ErlNifCharEncoding::Latin1 => {
            if name.chars().any(|ch| (ch as u32) > 0xFF) {
                0
            } else {
                name.chars().count() as c_uint
            }
        }
        ErlNifCharEncoding::Utf8 => name.len() as c_uint,
    };
    // SAFETY: `len` may be null (handled) or valid for one write.
    (unsafe { write_out(len, n) }) as c_int
}

/// `int enif_get_string(ErlNifEnv*, ERL_NIF_TERM list, char* buf, unsigned len, ErlNifCharEncoding);`
///
/// # Safety
///
/// `env` must be a live env; `buf` must be null or point at `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_get_string(
    _env: *mut crate::nif::types::ErlNifEnv,
    list: ERL_NIF_TERM,
    buf: *mut c_char,
    len: c_uint,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let len = len as usize;
    if buf.is_null() || len < 1 {
        return 0;
    }
    match encoding {
        ErlNifCharEncoding::Latin1 => {
            let mut n = 0usize;
            let mut cur = t(list);
            loop {
                if !prelude::is_list(cur) {
                    break;
                }
                // SAFETY: `cur` is a live cons cell.
                let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
                if !prelude::is_byte(head) {
                    // SAFETY: `n < len` here (the overflow return is below), so `buf[n]` is writable.
                    unsafe { buf.add(n).write(0) };
                    return 0;
                }
                // SAFETY: `n < len`, so index `n` is inside the caller's buffer.
                unsafe { buf.add(n).write(prelude::unsigned_value(head) as c_char) };
                n += 1;
                if n >= len {
                    // SAFETY: `n >= 1` and `n - 1 < len`.
                    unsafe { buf.add(n - 1).write(0) };
                    return -(n as c_int);
                }
                cur = tail;
            }
            if prelude::is_not_nil(cur) {
                // SAFETY: `n < len` (checked in the loop).
                unsafe { buf.add(n).write(0) };
                return 0;
            }
            // SAFETY: `n < len`; terminate the string.
            unsafe { buf.add(n).write(0) };
            (n + 1) as c_int
        }
        ErlNifCharEncoding::Utf8 => {
            let mut codepoints: Vec<char> = Vec::new();
            let mut cur = t(list);
            while prelude::is_list(cur) {
                // SAFETY: `cur` is a live cons cell.
                let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
                match decode_codepoint(head) {
                    Some(ch) => codepoints.push(ch),
                    None => {
                        // SAFETY: `len >= 1`.
                        unsafe { buf.write(0) };
                        return 0;
                    }
                }
                cur = tail;
            }
            if prelude::is_not_nil(cur) {
                // SAFETY: `len >= 1`.
                unsafe { buf.write(0) };
                return 0;
            }
            let encoded: Vec<u8> = codepoints.iter().collect::<String>().into_bytes();
            let mut n = 0usize;
            for &byte in &encoded {
                // SAFETY: `n < len` (the overflow branch returns first), so `buf[n]` is writable.
                unsafe { buf.add(n).write(byte as c_char) };
                n += 1;
                if n >= len {
                    // SAFETY: `n >= 1` and `n - 1 < len`.
                    unsafe { buf.add(n - 1).write(0) };
                    return -(n as c_int);
                }
            }
            // SAFETY: `n < len` here (the loop returns as soon as `n >= len`).
            unsafe { buf.add(n).write(0) };
            (n + 1) as c_int
        }
    }
}

/// Decode a list element as a Unicode codepoint (`None` for anything else).
fn decode_codepoint(term: Eterm) -> Option<char> {
    let value = if prelude::is_small(term) {
        let v = prelude::signed_value(term);
        if v < 0 {
            return None;
        }
        v as u64
    } else {
        term_to_uint64(term)?
    };
    u32::try_from(value).ok().and_then(char::from_u32)
}

/// `int enif_get_string_length(ErlNifEnv *env, ERL_NIF_TERM list, unsigned *len, ErlNifCharEncoding);`
///
/// # Safety
///
/// `len` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_string_length(
    _env: *mut crate::nif::types::ErlNifEnv,
    list: ERL_NIF_TERM,
    len: *mut c_uint,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let mut cur = t(list);
    if prelude::is_nil(cur) {
        // SAFETY: `len` may be null (handled) or valid for one write.
        return (unsafe { write_out(len, 0) }) as c_int;
    }
    match encoding {
        ErlNifCharEncoding::Latin1 => {
            let mut n = 0u32;
            while prelude::is_list(cur) {
                // SAFETY: `cur` is a live cons cell.
                let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
                if !prelude::is_byte(head) {
                    return 0;
                }
                n = match n.checked_add(1) {
                    Some(n) => n,
                    None => return 0,
                };
                cur = tail;
            }
            if prelude::is_not_nil(cur) {
                return 0;
            }
            // SAFETY: `len` may be null (handled) or valid for one write.
            (unsafe { write_out(len, n) }) as c_int
        }
        ErlNifCharEncoding::Utf8 => {
            let mut codepoints: Vec<char> = Vec::new();
            while prelude::is_list(cur) {
                // SAFETY: `cur` is a live cons cell.
                let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
                match decode_codepoint(head) {
                    Some(ch) => codepoints.push(ch),
                    None => return 0,
                }
                cur = tail;
            }
            if prelude::is_not_nil(cur) {
                return 0;
            }
            let bytes = codepoints.iter().collect::<String>().len();
            // SAFETY: `len` may be null (handled) or valid for one write.
            (unsafe { write_out(len, bytes as c_uint) }) as c_int
        }
    }
}

/// `int enif_get_tuple(ErlNifEnv* env, ERL_NIF_TERM tpl, int* arity, const ERL_NIF_TERM** array);`
///
/// # Safety
///
/// `arity` and `array` must be null or writable; the returned array borrows the tuple's storage.
#[no_mangle]
pub unsafe extern "C" fn enif_get_tuple(
    _env: *mut crate::nif::types::ErlNifEnv,
    tpl: ERL_NIF_TERM,
    arity: *mut c_int,
    array: *mut *const ERL_NIF_TERM,
) -> c_int {
    let term = t(tpl);
    if !is_term_word(term) {
        return 0;
    }
    // SAFETY: `is_tuple` only dereferences the header of a boxed term.
    if unsafe { prelude::is_not_tuple(term) } {
        return 0;
    }
    // SAFETY: `term` is a live tuple.
    let size = unsafe { prelude::tuple_size(term) };
    // SAFETY: `arity` may be null (handled) or valid for one write.
    if !(unsafe { write_out(arity, size as c_int) }) {
        return 0;
    }
    // SAFETY: `term` is a live tuple (checked above); `tuple_val` returns its first word and the
    // element array starts at the word after the header (`tuple_val(t) + 1`). `array` may be null
    // (handled) or valid for one write.
    let elements = unsafe { prelude::tuple_val(term).add(1) }
        .cast::<ERL_NIF_TERM>()
        .cast_const();
    // SAFETY: `array` may be null (handled) or valid for one write.
    (unsafe { write_out(array, elements) }) as c_int
}

/// `int enif_get_list_cell(ErlNifEnv* env, ERL_NIF_TERM term, ERL_NIF_TERM* head, ERL_NIF_TERM* tail);`
///
/// # Safety
///
/// `head` and `tail` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_list_cell(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    head: *mut ERL_NIF_TERM,
    tail: *mut ERL_NIF_TERM,
) -> c_int {
    let term = t(term);
    if prelude::is_not_list(term) {
        return 0;
    }
    // SAFETY: `term` is a live cons cell.
    let (car, cdr) = unsafe { (prelude::car(term), prelude::cdr(term)) };
    // SAFETY: both out-pointers may be null (handled) or valid for one write; both are written
    // (no short-circuit) so a null `head` never suppresses the `tail` write.
    let wrote_head = unsafe { write_out(head, n(car)) };
    let wrote_tail = unsafe { write_out(tail, n(cdr)) };
    (wrote_head && wrote_tail) as c_int
}

/// `int enif_get_list_length(ErlNifEnv* env, ERL_NIF_TERM term, unsigned* len);`
///
/// # Safety
///
/// `len` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_list_length(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    len: *mut c_uint,
) -> c_int {
    let mut cur = t(term);
    let mut n: u64 = 0;
    loop {
        if prelude::is_nil(cur) {
            // SAFETY: `len` may be null (handled) or valid for one write.
            return (unsafe { write_out(len, n as c_uint) }) as c_int;
        }
        if !prelude::is_list(cur) {
            return 0;
        }
        n += 1;
        if n > c_uint::MAX as u64 {
            return 0;
        }
        // SAFETY: `cur` is a live cons cell.
        cur = unsafe { prelude::cdr(cur) };
    }
}

/// `int enif_get_map_size(ErlNifEnv *env, ERL_NIF_TERM term, size_t *size);`
///
/// # Safety
///
/// `size` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_map_size(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    size: *mut size_t,
) -> c_int {
    if !is_term_word(t(term)) {
        return 0;
    }
    match prelude::map_size(t(term), &no_atoms()) {
        // SAFETY: `size` may be null (handled) or valid for one write.
        Ok(n) => (unsafe { write_out(size, n) }) as c_int,
        Err(_) => 0,
    }
}

/// `int enif_get_map_value(ErlNifEnv *env, ERL_NIF_TERM map, ERL_NIF_TERM key, ERL_NIF_TERM* value);`
///
/// # Safety
///
/// `value` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_map_value(
    _env: *mut crate::nif::types::ErlNifEnv,
    map: ERL_NIF_TERM,
    key: ERL_NIF_TERM,
    value: *mut ERL_NIF_TERM,
) -> c_int {
    match prelude::map_get(t(map), t(key), &no_atoms()) {
        Ok(Some(v)) => {
            // SAFETY: `value` may be null (handled) or valid for one write.
            (unsafe { write_out(value, n(v)) }) as c_int
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Predicates
// ---------------------------------------------------------------------------------------------

/// `int enif_is_atom(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term; no pointer out-parameters.
#[no_mangle]
pub unsafe extern "C" fn enif_is_atom(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    prelude::is_atom(t(term)) as c_int
}

/// `int enif_is_binary(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_binary(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_binary` only dereferences the header of a boxed term.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_binary(t(term)) }) as c_int
}

/// `int enif_is_ref(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_ref(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    is_ref(t(term)) as c_int
}

/// `int enif_is_fun(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_fun(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_any_fun` only dereferences the header of a boxed term.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_any_fun(t(term)) }) as c_int
}

/// `int enif_is_pid(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_pid(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    is_pid(t(term)) as c_int
}

/// `int enif_is_port(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_port(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    is_port(t(term)) as c_int
}

/// `int enif_is_empty_list(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_empty_list(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    prelude::is_nil(t(term)) as c_int
}

/// `int enif_is_list(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// True for a cons cell **or** `[]` (the C `is_list(term) || is_nil(term)`).
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_list(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    let term = t(term);
    (prelude::is_list(term) || prelude::is_nil(term)) as c_int
}

/// `int enif_is_tuple(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_tuple(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_tuple` only dereferences the header of a boxed term.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_tuple(t(term)) }) as c_int
}

/// `int enif_is_map(ErlNifEnv* env, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_map(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_map` only dereferences the header of a boxed term.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_map(t(term)) }) as c_int
}

/// `int enif_is_number(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_number(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_number` only inspects tags/headers, never panics.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_number(t(term)) }) as c_int
}

/// `int enif_is_exception(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_is_exception(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    (e.pending_exception().is_some() && prelude::is_non_value(t(term))) as c_int
}

/// `int enif_is_identical(ERL_NIF_TERM lhs, ERL_NIF_TERM rhs);`
///
/// # Safety
///
/// Both arguments are any terms; no pointers.
#[no_mangle]
pub unsafe extern "C" fn enif_is_identical(lhs: ERL_NIF_TERM, rhs: ERL_NIF_TERM) -> c_int {
    let (a, b) = (t(lhs), t(rhs));
    if a.raw() == b.raw() {
        // The C `EQ` macro's fast path: bit-identical words are equal terms.
        return 1;
    }
    prelude::eq(a, b, &no_atoms()) as c_int
}

/// `int enif_compare(ERL_NIF_TERM lhs, ERL_NIF_TERM rhs);` — `-1`, `0` or `1`.
///
/// # Safety
///
/// Both arguments are any terms; no pointers.
#[no_mangle]
pub unsafe extern "C" fn enif_compare(lhs: ERL_NIF_TERM, rhs: ERL_NIF_TERM) -> c_int {
    let (a, b) = (t(lhs), t(rhs));
    if a.raw() == b.raw() {
        return 0;
    }
    match prelude::cmp(a, b, &no_atoms()) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// `ErlNifUInt64 enif_hash(ErlNifHash type, ERL_NIF_TERM term, ErlNifUInt64 salt);`
///
/// # Safety
///
/// `term` is any term; no pointers.
#[no_mangle]
pub unsafe extern "C" fn enif_hash(
    hash_type: ErlNifHash,
    term: ERL_NIF_TERM,
    salt: u64,
) -> u64 {
    match hash_type {
        // An implementation-defined internal hash; salted, like the C.
        ErlNifHash::InternalHash => prelude::term_hash(t(term), &no_atoms()) ^ salt,
        // `erlang:phash2/1` — the low 27 bits, unsalted (as the C ignores the salt here).
        ErlNifHash::Phash2 => prelude::term_hash(t(term), &no_atoms()) & ((1 << 27) - 1),
    }
}

/// `size_t enif_term_size(ERL_NIF_TERM term);` — the term's heap size in bytes.
///
/// # Safety
///
/// `term` is any term; no pointers.
#[no_mangle]
pub unsafe extern "C" fn enif_term_size(term: ERL_NIF_TERM) -> size_t {
    if !is_term_word(t(term)) {
        return 0;
    }
    prelude::size_object(t(term)) * core::mem::size_of::<Eterm>()
}

/// `ErlNifTermType enif_term_type(ErlNifEnv* env, ERL_NIF_TERM term);`
///
/// Total: it never aborts. `[]` reports `List` (consistent with [`enif_is_list`]) and a reference
/// reports `Reference`; the C's `default` arm is exactly `MissingDefaultCaseReadTheManual`.
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_term_type(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> ErlNifTermType {
    let term = t(term);
    if !is_term_word(term) {
        return ErlNifTermType::MissingDefaultCaseReadTheManual;
    }
    if prelude::is_atom(term) {
        return ErlNifTermType::Atom;
    }
    if prelude::is_nil(term) {
        return ErlNifTermType::List;
    }
    if prelude::is_small(term) || unsafe { prelude::is_big(term) } {
        return ErlNifTermType::Integer;
    }
    // SAFETY: the remaining predicates only dereference the header of a boxed term (and each checks
    // `is_boxed` first), so they are total over arbitrary terms.
    unsafe {
        if prelude::is_float(term) {
            return ErlNifTermType::Float;
        }
        if prelude::is_bitstring(term) {
            return ErlNifTermType::Bitstring;
        }
        if prelude::is_list(term) {
            return ErlNifTermType::List;
        }
        if prelude::is_tuple(term) {
            return ErlNifTermType::Tuple;
        }
        if prelude::is_map(term) {
            return ErlNifTermType::Map;
        }
        if prelude::is_any_fun(term) {
            return ErlNifTermType::Fun;
        }
    }
    if prelude::is_internal_pid(term) || unsafe { prelude::is_external_pid(term) } {
        return ErlNifTermType::Pid;
    }
    if prelude::is_internal_port(term) || unsafe { prelude::is_external_port(term) } {
        return ErlNifTermType::Port;
    }
    if is_ref(term) {
        return ErlNifTermType::Reference;
    }
    ErlNifTermType::MissingDefaultCaseReadTheManual
}

/// `int enif_get_local_pid(ErlNifEnv* env, ERL_NIF_TERM, ErlNifPid* pid);`
///
/// # Safety
///
/// `pid` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_local_pid(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    pid: *mut ErlNifPid,
) -> c_int {
    let term = t(term);
    if !prelude::is_internal_pid(term) {
        return 0;
    }
    // SAFETY: `pid` may be null (handled) or valid for one write.
    (unsafe { write_out(pid, ErlNifPid { pid: n(term) }) }) as c_int
}

/// `int enif_get_local_port(ErlNifEnv* env, ERL_NIF_TERM, ErlNifPort* port_id);`
///
/// # Safety
///
/// `port_id` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_local_port(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    port_id: *mut ErlNifPort,
) -> c_int {
    let term = t(term);
    if !prelude::is_internal_port(term) {
        return 0;
    }
    // SAFETY: `port_id` may be null (handled) or valid for one write.
    (unsafe { write_out(port_id, ErlNifPort { port_id: n(term) }) }) as c_int
}

/// `ErlNifPid* enif_self(ErlNifEnv* caller_env, ErlNifPid* pid);`
///
/// Returns `pid` on success, `NULL` when the caller has no process (`NifHost::self_pid` = `None`).
///
/// # Safety
///
/// `env` must be a live env; `pid` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_self(
    env: *mut crate::nif::types::ErlNifEnv,
    pid: *mut ErlNifPid,
) -> *mut ErlNifPid {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return core::ptr::null_mut();
    };
    if pid.is_null() {
        return core::ptr::null_mut();
    }
    let Some(self_pid) = e.host().self_pid() else {
        return core::ptr::null_mut();
    };
    // SAFETY: `pid` is non-null and writable (checked).
    unsafe { pid.write(ErlNifPid { pid: n(self_pid) }) };
    pid
}

/// `void enif_set_pid_undefined(ErlNifPid* pid);`
///
/// # Safety
///
/// `pid` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_set_pid_undefined(pid: *mut ErlNifPid) {
    // SAFETY: `pid` may be null (handled) or valid for one write.
    unsafe { write_out(pid, ErlNifPid { pid: UNDEFINED_PID }) };
}

/// `int enif_is_pid_undefined(const ErlNifPid* pid);`
///
/// # Safety
///
/// `pid` must be null or point at a readable `ErlNifPid`.
#[no_mangle]
pub unsafe extern "C" fn enif_is_pid_undefined(pid: *const ErlNifPid) -> c_int {
    if pid.is_null() {
        return 0;
    }
    // SAFETY: `pid` is non-null and readable (caller contract).
    (unsafe { (*pid).pid.raw() } == UNDEFINED_PID.raw()) as c_int
}

/// `int enif_get_atom_cache_index(ErlNifEnv *env, ERL_NIF_TERM atom, unsigned *index);`
///
/// This runtime exposes no cached atom indices, so no atom has one (always `0`).
///
/// # Safety
///
/// `index` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_atom_cache_index(
    _env: *mut crate::nif::types::ErlNifEnv,
    _atom: ERL_NIF_TERM,
    _index: *mut c_uint,
) -> c_int {
    0
}

/// `unsigned enif_max_atom_cache_index(void);` — no cached indices exist (`0`).
///
/// # Safety
///
/// No pointers; always safe.
#[no_mangle]
pub unsafe extern "C" fn enif_max_atom_cache_index() -> c_uint {
    0
}

// ---------------------------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_TERM enif_make_int(ErlNifEnv* env, int i);`
///
/// # Safety
///
/// `env` may be null (a small int needs no heap).
#[no_mangle]
pub unsafe extern "C" fn enif_make_int(
    _env: *mut crate::nif::types::ErlNifEnv,
    i: c_int,
) -> ERL_NIF_TERM {
    // An `int` always fits a 60-bit small integer.
    n(prelude::make_small(i as i64))
}

/// `ERL_NIF_TERM enif_make_uint(ErlNifEnv* env, unsigned i);`
///
/// # Safety
///
/// `env` may be null (a small int needs no heap).
#[no_mangle]
pub unsafe extern "C" fn enif_make_uint(
    _env: *mut crate::nif::types::ErlNifEnv,
    i: c_uint,
) -> ERL_NIF_TERM {
    n(prelude::make_small(i as i64))
}

/// `make_small` when the value fits, else a one-limb bignum of the right sign.
fn small_or_big(env: &mut NifEnv, negative: bool, magnitude: u64) -> Eterm {
    let fits_small = if negative {
        magnitude <= prelude::MIN_SMALL.unsigned_abs()
    } else {
        magnitude <= prelude::MAX_SMALL as u64
    };
    if fits_small {
        let value = if negative {
            (magnitude as i64).wrapping_neg()
        } else {
            magnitude as i64
        };
        return prelude::make_small(value);
    }
    // SAFETY: `big_size_words(1)` sizes the `[hdr][digit]` block.
    let Some(hp) = (unsafe { alloc(env, prelude::big_size_words(1)) }) else {
        return badarg_atom(env);
    };
    // SAFETY: `hp` heads the reserved block and `digits` has exactly one (non-zero) limb.
    unsafe { prelude::write_big(hp, negative, &[magnitude]) }
}

/// `ERL_NIF_TERM enif_make_long(ErlNifEnv* env, long i);`
///
/// # Safety
///
/// `env` must be a live env when `i` does not fit a small integer.
#[no_mangle]
pub unsafe extern "C" fn enif_make_long(
    env: *mut crate::nif::types::ErlNifEnv,
    i: c_long,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    n(small_or_big(e, i < 0, (i as i64).unsigned_abs()))
}

/// `ERL_NIF_TERM enif_make_ulong(ErlNifEnv* env, unsigned long i);`
///
/// # Safety
///
/// `env` must be a live env when `i` does not fit a small integer.
#[no_mangle]
pub unsafe extern "C" fn enif_make_ulong(
    env: *mut crate::nif::types::ErlNifEnv,
    i: c_ulong,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    n(small_or_big(e, false, i as u64))
}

/// `ERL_NIF_TERM enif_make_int64(ErlNifEnv* env, ErlNifSInt64 i);` (LP64 alias of `enif_make_long`).
///
/// # Safety
///
/// As [`enif_make_long`].
#[no_mangle]
pub unsafe extern "C" fn enif_make_int64(
    env: *mut crate::nif::types::ErlNifEnv,
    i: i64,
) -> ERL_NIF_TERM {
    // SAFETY: `c_long` is `i64` on LP64; same contract.
    unsafe { enif_make_long(env, i as c_long) }
}

/// `ERL_NIF_TERM enif_make_uint64(ErlNifEnv* env, ErlNifUInt64 i);` (LP64 alias of `enif_make_ulong`).
///
/// # Safety
///
/// As [`enif_make_ulong`].
#[no_mangle]
pub unsafe extern "C" fn enif_make_uint64(
    env: *mut crate::nif::types::ErlNifEnv,
    i: u64,
) -> ERL_NIF_TERM {
    // SAFETY: `c_ulong` is `u64` on LP64; same contract.
    unsafe { enif_make_ulong(env, i as c_ulong) }
}

/// `ERL_NIF_TERM enif_make_double(ErlNifEnv* env, double d);`
///
/// A non-finite value yields `badarg` (as the C does).
///
/// # Safety
///
/// `env` must be a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_double(
    env: *mut crate::nif::types::ErlNifEnv,
    d: c_double,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    if !d.is_finite() {
        // SAFETY: `e` is a live env.
        return unsafe { enif_make_badarg(env) };
    }
    // SAFETY: `FLOAT_SIZE_WORDS` sizes the `[HEADER_FLONUM][bits]` block.
    let Some(hp) = (unsafe { alloc(e, prelude::FLOAT_SIZE_WORDS) }) else {
        return bare_badarg();
    };
    // SAFETY: `hp` heads the reserved 2-word block.
    n(unsafe { prelude::write_float(hp, d) })
}

/// `ERL_NIF_TERM enif_make_atom(ErlNifEnv* env, const char* name);`
///
/// # Safety
///
/// `env` must be a live env; `name` must be null or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn enif_make_atom(
    env: *mut crate::nif::types::ErlNifEnv,
    name: *const c_char,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let Some(bytes) = (unsafe { cstr(name) }) else {
        // SAFETY: `e` is a live env.
        return unsafe { enif_make_badarg(env) };
    };
    match core::str::from_utf8(bytes).ok().and_then(|s| e.host().intern_atom(s)) {
        Some(atom) => n(atom),
        // SAFETY: `e` is a live env.
        None => unsafe { enif_make_badarg(env) },
    }
}

/// `ERL_NIF_TERM enif_make_atom_len(ErlNifEnv* env, const char* name, size_t len);`
///
/// # Safety
///
/// `env` must be a live env; `name` must point at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_make_atom_len(
    env: *mut crate::nif::types::ErlNifEnv,
    name: *const c_char,
    len: size_t,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let Some(bytes) = (unsafe { cbuf(name, len) }) else {
        // SAFETY: `e` is a live env.
        return unsafe { enif_make_badarg(env) };
    };
    match core::str::from_utf8(bytes).ok().and_then(|s| e.host().intern_atom(s)) {
        Some(atom) => n(atom),
        // SAFETY: `e` is a live env.
        None => unsafe { enif_make_badarg(env) },
    }
}

/// Decode a name into a Rust `String` per the requested encoding (`None` for invalid input).
fn decode_name(bytes: &[u8], encoding: ErlNifCharEncoding) -> Option<String> {
    match encoding {
        // Latin-1: every byte is a codepoint, so the mapping is total.
        ErlNifCharEncoding::Latin1 => Some(bytes.iter().map(|&b| b as char).collect()),
        ErlNifCharEncoding::Utf8 => core::str::from_utf8(bytes).ok().map(str::to_string),
    }
}

/// `int enif_make_new_atom(ErlNifEnv *env, const char *name, ERL_NIF_TERM *atom, ErlNifCharEncoding encoding);`
///
/// # Safety
///
/// `env` must be a live env; `name` must be null or NUL-terminated; `atom` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_new_atom(
    env: *mut crate::nif::types::ErlNifEnv,
    name: *const c_char,
    atom: *mut ERL_NIF_TERM,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let Some(bytes) = (unsafe { cstr(name) }) else {
        return 0;
    };
    match decode_name(bytes, encoding).and_then(|s| e.host().intern_atom(&s)) {
        // SAFETY: `atom` may be null (handled) or valid for one write.
        Some(term) => (unsafe { write_out(atom, n(term)) }) as c_int,
        None => 0,
    }
}

/// `int enif_make_new_atom_len(ErlNifEnv *env, const char *name, size_t len, ERL_NIF_TERM *atom, ErlNifCharEncoding encoding);`
///
/// # Safety
///
/// `env` must be a live env; `name` must point at `len` readable bytes; `atom` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_new_atom_len(
    env: *mut crate::nif::types::ErlNifEnv,
    name: *const c_char,
    len: size_t,
    atom: *mut ERL_NIF_TERM,
    encoding: ErlNifCharEncoding,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let Some(bytes) = (unsafe { cbuf(name, len) }) else {
        return 0;
    };
    match decode_name(bytes, encoding).and_then(|s| e.host().intern_atom(&s)) {
        // SAFETY: `atom` may be null (handled) or valid for one write.
        Some(term) => (unsafe { write_out(atom, n(term)) }) as c_int,
        None => 0,
    }
}

/// `ERL_NIF_TERM enif_make_tuple_from_array(ErlNifEnv*, const ERL_NIF_TERM arr[], unsigned cnt);`
///
/// # Safety
///
/// `env` must be a live env; `arr` must point at `cnt` readable terms.
#[no_mangle]
pub unsafe extern "C" fn enif_make_tuple_from_array(
    env: *mut crate::nif::types::ErlNifEnv,
    arr: *const ERL_NIF_TERM,
    cnt: c_uint,
) -> ERL_NIF_TERM {
    let mut terms = Vec::with_capacity(cnt as usize);
    for i in 0..cnt as usize {
        if arr.is_null() {
            return bare_badarg();
        }
        // SAFETY: the caller declares `cnt` readable terms at `arr`.
        terms.push(t(unsafe { *arr.add(i) }));
    }
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    if terms.is_empty() {
        return n(prelude::tuple0());
    }
    // SAFETY: `1 + cnt` words hold the arity header plus the elements.
    let Some(hp) = (unsafe { alloc(e, 1 + terms.len()) }) else {
        return bare_badarg();
    };
    // SAFETY: `hp` heads the reserved block and every word of it is written here.
    unsafe {
        *hp = prelude::make_arityval(terms.len());
        for (i, term) in terms.iter().enumerate() {
            *hp.add(1 + i) = *term;
        }
        n(prelude::make_tuple(hp))
    }
}

/// `ERL_NIF_TERM enif_make_list_cell(ErlNifEnv* env, ERL_NIF_TERM car, ERL_NIF_TERM cdr);`
///
/// # Safety
///
/// `env` must be a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_list_cell(
    env: *mut crate::nif::types::ErlNifEnv,
    car: ERL_NIF_TERM,
    cdr: ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    match e.heap().cons(t(car), t(cdr)) {
        Ok(cell) => n(cell),
        Err(_) => bare_badarg(),
    }
}

/// `ERL_NIF_TERM enif_make_list_from_array(ErlNifEnv*, const ERL_NIF_TERM arr[], unsigned cnt);`
///
/// # Safety
///
/// `env` must be a live env; `arr` must point at `cnt` readable terms.
#[no_mangle]
pub unsafe extern "C" fn enif_make_list_from_array(
    env: *mut crate::nif::types::ErlNifEnv,
    arr: *const ERL_NIF_TERM,
    cnt: c_uint,
) -> ERL_NIF_TERM {
    let mut terms = Vec::with_capacity(cnt as usize);
    for i in 0..cnt as usize {
        if arr.is_null() {
            return bare_badarg();
        }
        // SAFETY: the caller declares `cnt` readable terms at `arr`.
        terms.push(t(unsafe { *arr.add(i) }));
    }
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let mut acc = NIL;
    for term in terms.into_iter().rev() {
        acc = match e.heap().cons(term, acc) {
            Ok(cell) => cell,
            Err(_) => return bare_badarg(),
        };
    }
    n(acc)
}

/// `int enif_make_reverse_list(ErlNifEnv*, ERL_NIF_TERM term, ERL_NIF_TERM *list);`
///
/// # Safety
///
/// `env` must be a live env; `list` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_reverse_list(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    list: *mut ERL_NIF_TERM,
) -> c_int {
    if list.is_null() {
        return 0;
    }
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let mut cur = t(term);
    let mut acc = NIL;
    while prelude::is_list(cur) {
        // SAFETY: `cur` is a live cons cell.
        let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
        acc = match e.heap().cons(head, acc) {
            Ok(cell) => cell,
            Err(_) => return 0,
        };
        cur = tail;
    }
    if prelude::is_not_nil(cur) {
        return 0;
    }
    // SAFETY: `list` is non-null and writable (checked).
    unsafe { list.write(n(acc)) };
    1
}

/// The list of small ints (LATIN1 bytes) or codepoints (UTF8) for `bytes`.
fn string_list(env: &mut NifEnv, bytes: &[u8], encoding: ErlNifCharEncoding) -> Option<Eterm> {
    let chars: Vec<i64> = match encoding {
        ErlNifCharEncoding::Latin1 => bytes.iter().map(|&b| b as i64).collect(),
        ErlNifCharEncoding::Utf8 => {
            let text = core::str::from_utf8(bytes).ok()?;
            text.chars().map(|c| c as i64).collect()
        }
    };
    let mut acc = NIL;
    for value in chars.into_iter().rev() {
        acc = env.heap().cons(prelude::make_small(value), acc).ok()?;
    }
    Some(acc)
}

/// `ERL_NIF_TERM enif_make_string(ErlNifEnv* env, const char* string, ErlNifCharEncoding);`
///
/// # Safety
///
/// `env` must be a live env; `string` must be null or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn enif_make_string(
    env: *mut crate::nif::types::ErlNifEnv,
    string: *const c_char,
    encoding: ErlNifCharEncoding,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let Some(bytes) = (unsafe { cstr(string) }) else {
        // SAFETY: `e` is a live env.
        return unsafe { enif_make_badarg(env) };
    };
    match string_list(e, bytes, encoding) {
        Some(list) => n(list),
        // SAFETY: `e` is a live env.
        None => unsafe { enif_make_badarg(env) },
    }
}

/// `ERL_NIF_TERM enif_make_string_len(ErlNifEnv* env, const char* string, size_t len, ErlNifCharEncoding);`
///
/// # Safety
///
/// `env` must be a live env; `string` must point at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_make_string_len(
    env: *mut crate::nif::types::ErlNifEnv,
    string: *const c_char,
    len: size_t,
    encoding: ErlNifCharEncoding,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let Some(bytes) = (unsafe { cbuf(string, len) }) else {
        // SAFETY: `e` is a live env.
        return unsafe { enif_make_badarg(env) };
    };
    match string_list(e, bytes, encoding) {
        Some(list) => n(list),
        // SAFETY: `e` is a live env.
        None => unsafe { enif_make_badarg(env) },
    }
}

/// `ERL_NIF_TERM enif_make_ref(ErlNifEnv* env);`
///
/// When the host provides no `make_ref` (the default), the `badarg` exception is raised and the
/// `badarg` atom is returned.
///
/// # Safety
///
/// `env` must be a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_ref(env: *mut crate::nif::types::ErlNifEnv) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    match e.host().make_ref() {
        Some(reference) => n(reference),
        // SAFETY: `e` is a live env.
        None => unsafe { enif_make_badarg(env) },
    }
}

// ---------------------------------------------------------------------------------------------
// Maps
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_TERM enif_make_new_map(ErlNifEnv* env);`
///
/// # Safety
///
/// `env` must be a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_new_map(env: *mut crate::nif::types::ErlNifEnv) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    // SAFETY: `flatmap_size_words(0)` sizes the empty flat map.
    let Some(hp) = (unsafe { alloc(e, prelude::flatmap_size_words(0)) }) else {
        return bare_badarg();
    };
    // SAFETY: `hp` heads the reserved block; an empty pair list leaves it fully written.
    n(unsafe { prelude::write_flatmap(hp, &[]) })
}

/// `int enif_make_map_put(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM value, ERL_NIF_TERM* map_out);`
///
/// # Safety
///
/// `env` must be a live env; `map_out` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_map_put(
    env: *mut crate::nif::types::ErlNifEnv,
    map_in: ERL_NIF_TERM,
    key: ERL_NIF_TERM,
    value: ERL_NIF_TERM,
    map_out: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let map = t(map_in);
    let Ok(size) = prelude::map_size(map, &no_atoms()) else {
        return 0;
    };
    // SAFETY: `map_words_capacity(size + 1)` sizes the result of a put.
    let Some(hp) = (unsafe { alloc(e, prelude::map_words_capacity(size + 1)) }) else {
        return 0;
    };
    match unsafe { prelude::map_put(hp, map, t(key), t(value), &no_atoms()) } {
        // SAFETY: `map_out` may be null (handled) or valid for one write.
        Ok(out) => (unsafe { write_out(map_out, n(out)) }) as c_int,
        Err(_) => 0,
    }
}

/// `int enif_make_map_update(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM value, ERL_NIF_TERM* map_out);`
///
/// Fails (`0`) when `key` is absent, exactly as the C does.
///
/// # Safety
///
/// `env` must be a live env; `map_out` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_map_update(
    env: *mut crate::nif::types::ErlNifEnv,
    map_in: ERL_NIF_TERM,
    key: ERL_NIF_TERM,
    value: ERL_NIF_TERM,
    map_out: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let map = t(map_in);
    let Ok(Some(_)) = prelude::map_get(map, t(key), &no_atoms()) else {
        return 0;
    };
    let Ok(size) = prelude::map_size(map, &no_atoms()) else {
        return 0;
    };
    // SAFETY: `map_words_capacity(size + 1)` sizes the result of a put.
    let Some(hp) = (unsafe { alloc(e, prelude::map_words_capacity(size + 1)) }) else {
        return 0;
    };
    match unsafe { prelude::map_put(hp, map, t(key), t(value), &no_atoms()) } {
        // SAFETY: `map_out` may be null (handled) or valid for one write.
        Ok(out) => (unsafe { write_out(map_out, n(out)) }) as c_int,
        Err(_) => 0,
    }
}

/// `int enif_make_map_remove(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM* map_out);`
///
/// Returns `0` **only** when `map_in` is not a map; for any map it returns `1`, leaving `map_out`
/// equal to `map_in` when `key` is absent. This is the C's behaviour (`erl_nif.c:3759`): it ignores
/// `erts_maps_take`'s found/absent result and returns `1` unconditionally after the `is_map` guard.
///
/// # Safety
///
/// `env` must be a live env; `map_out` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_map_remove(
    env: *mut crate::nif::types::ErlNifEnv,
    map_in: ERL_NIF_TERM,
    key: ERL_NIF_TERM,
    map_out: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let map = t(map_in);
    let Ok(size) = prelude::map_size(map, &no_atoms()) else {
        return 0;
    };
    // SAFETY: `map_words_capacity(size)` sizes the result of a remove — the pair count only shrinks.
    let Some(hp) = (unsafe { alloc(e, prelude::map_words_capacity(size)) }) else {
        return 0;
    };
    match unsafe { prelude::map_remove(hp, map, t(key), &no_atoms()) } {
        // SAFETY: `map_out` may be null (handled) or valid for one write.
        Ok(out) => (unsafe { write_out(map_out, n(out)) }) as c_int,
        Err(_) => 0,
    }
}

/// `int enif_make_map_from_arrays(ErlNifEnv *env, ERL_NIF_TERM keys[], ERL_NIF_TERM values[], size_t cnt, ERL_NIF_TERM *map_out);`
///
/// # Safety
///
/// `env` must be a live env; `keys`/`values` must point at `cnt` readable terms; `map_out` must be
/// null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_map_from_arrays(
    env: *mut crate::nif::types::ErlNifEnv,
    keys: *mut ERL_NIF_TERM,
    values: *mut ERL_NIF_TERM,
    cnt: size_t,
    map_out: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if cnt > 0 && (keys.is_null() || values.is_null()) {
        return 0;
    }
    let mut pairs: Vec<(Eterm, Eterm)> = Vec::with_capacity(cnt);
    for i in 0..cnt {
        // SAFETY: the caller declares `cnt` readable terms in each array.
        let (k, v) = unsafe { (*keys.add(i), *values.add(i)) };
        pairs.push((t(k), t(v)));
    }
    // SAFETY: `map_words_capacity(pairs.len())` sizes the map built from deduplicated pairs.
    let Some(hp) = (unsafe { alloc(e, prelude::map_words_capacity(pairs.len())) }) else {
        return 0;
    };
    match unsafe { prelude::write_map(hp, &pairs, &no_atoms()) } {
        // SAFETY: `map_out` may be null (handled) or valid for one write.
        Ok(map) => (unsafe { write_out(map_out, n(map)) }) as c_int,
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Exceptions / misc
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_TERM enif_make_badarg(ErlNifEnv* env);`
///
/// Raises `badarg` and returns the `badarg` atom.
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_badarg(env: *mut crate::nif::types::ErlNifEnv) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let reason = badarg_atom(e);
    e.set_exception(reason);
    n(reason)
}

/// `ERL_NIF_TERM enif_raise_exception(ErlNifEnv *env, ERL_NIF_TERM reason);`
///
/// Records `reason` as the pending exception and returns it.
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_raise_exception(
    env: *mut crate::nif::types::ErlNifEnv,
    reason: ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    if let Some(e) = unsafe { env_mut(env) } {
        e.set_exception(t(reason));
    }
    reason
}

/// `int enif_has_pending_exception(ErlNifEnv *env, ERL_NIF_TERM* reason);`
///
/// # Safety
///
/// `env` must be null or a live env; `reason` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_has_pending_exception(
    env: *mut crate::nif::types::ErlNifEnv,
    reason: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    match e.pending_exception() {
        Some(pending) => {
            if !reason.is_null() {
                // SAFETY: `reason` is non-null and writable (checked).
                unsafe { reason.write(n(pending)) };
            }
            1
        }
        None => 0,
    }
}

/// `void enif_clear_env(ErlNifEnv* env);`
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_clear_env(env: *mut crate::nif::types::ErlNifEnv) {
    if let Some(e) = unsafe { env_mut(env) } {
        e.clear();
    }
}

// ---------------------------------------------------------------------------------------------
// The `#[used]` table
// ---------------------------------------------------------------------------------------------

/// The runtime's populated `enif_*` table.
///
/// `#[used]` keeps this static (and hence the `#[no_mangle]` functions it names) in the linkage, so a
/// NIF `.so` loaded into the process can resolve them. Slots left `None` are documented in the module
/// header.
#[used]
static ENIF_API: ErlNifApi = ErlNifApi {
    enif_alloc: Some(enif_alloc),
    enif_free: Some(enif_free),
    enif_realloc: Some(enif_realloc),
    enif_is_atom: Some(enif_is_atom),
    enif_is_binary: Some(enif_is_binary),
    enif_is_ref: Some(enif_is_ref),
    enif_inspect_binary: Some(enif_inspect_binary),
    enif_inspect_iolist_as_binary: Some(enif_inspect_iolist_as_binary),
    enif_alloc_binary: Some(enif_alloc_binary),
    enif_realloc_binary: Some(enif_realloc_binary),
    enif_release_binary: Some(enif_release_binary),
    enif_get_int: Some(enif_get_int),
    enif_get_uint: Some(enif_get_uint),
    enif_get_long: Some(enif_get_long),
    enif_get_ulong: Some(enif_get_ulong),
    enif_get_double: Some(enif_get_double),
    enif_get_list_cell: Some(enif_get_list_cell),
    enif_get_tuple: Some(enif_get_tuple),
    enif_get_list_length: Some(enif_get_list_length),
    enif_get_string: Some(enif_get_string),
    enif_get_string_length: Some(enif_get_string_length),
    enif_get_atom: Some(enif_get_atom),
    enif_get_atom_length: Some(enif_get_atom_length),
    enif_is_identical: Some(enif_is_identical),
    enif_compare: Some(enif_compare),
    enif_make_binary: Some(enif_make_binary),
    enif_make_sub_binary: Some(enif_make_sub_binary),
    enif_make_badarg: Some(enif_make_badarg),
    enif_raise_exception: Some(enif_raise_exception),
    enif_has_pending_exception: Some(enif_has_pending_exception),
    enif_clear_env: Some(enif_clear_env),
    enif_make_int: Some(enif_make_int),
    enif_make_uint: Some(enif_make_uint),
    enif_make_long: Some(enif_make_long),
    enif_make_ulong: Some(enif_make_ulong),
    enif_make_double: Some(enif_make_double),
    enif_make_atom: Some(enif_make_atom),
    enif_make_atom_len: Some(enif_make_atom_len),
    enif_make_new_atom: Some(enif_make_new_atom),
    enif_make_new_atom_len: Some(enif_make_new_atom_len),
    enif_make_list_cell: Some(enif_make_list_cell),
    enif_make_string: Some(enif_make_string),
    enif_make_string_len: Some(enif_make_string_len),
    enif_make_ref: Some(enif_make_ref),
    enif_make_tuple_from_array: Some(enif_make_tuple_from_array),
    enif_make_list_from_array: Some(enif_make_list_from_array),
    enif_make_reverse_list: Some(enif_make_reverse_list),
    enif_is_empty_list: Some(enif_is_empty_list),
    enif_is_fun: Some(enif_is_fun),
    enif_is_pid: Some(enif_is_pid),
    enif_is_port: Some(enif_is_port),
    enif_is_list: Some(enif_is_list),
    enif_is_tuple: Some(enif_is_tuple),
    enif_is_map: Some(enif_is_map),
    enif_is_number: Some(enif_is_number),
    enif_is_exception: Some(enif_is_exception),
    enif_get_map_size: Some(enif_get_map_size),
    enif_get_map_value: Some(enif_get_map_value),
    enif_make_new_map: Some(enif_make_new_map),
    enif_make_map_from_arrays: Some(enif_make_map_from_arrays),
    enif_make_map_put: Some(enif_make_map_put),
    enif_make_map_update: Some(enif_make_map_update),
    enif_make_map_remove: Some(enif_make_map_remove),
    enif_make_new_binary: Some(enif_make_new_binary),
    enif_term_to_binary: Some(enif_term_to_binary),
    enif_binary_to_term: Some(enif_binary_to_term),
    enif_self: Some(enif_self),
    enif_get_local_pid: Some(enif_get_local_pid),
    enif_get_local_port: Some(enif_get_local_port),
    enif_set_pid_undefined: Some(enif_set_pid_undefined),
    enif_is_pid_undefined: Some(enif_is_pid_undefined),
    enif_term_type: Some(enif_term_type),
    enif_term_size: Some(enif_term_size),
    enif_hash: Some(enif_hash),
    enif_get_atom_cache_index: Some(enif_get_atom_cache_index),
    enif_max_atom_cache_index: Some(enif_max_atom_cache_index),
    ..ErlNifApi::empty()
};

/// The populated `enif_*` table (see [`ENIF_API`]).
pub fn enif_api_table() -> &'static ErlNifApi {
    &ENIF_API
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{atom_val, is_atom, make_small};

    use crate::nif::host::NifHost;
    struct TestHost {
        heap: Heap,
        atoms: AtomTable,
    }

    impl NifHost for TestHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(prelude::make_atom(self.atoms.intern(name)))
        }
        fn atom_name(&mut self, atom: Eterm) -> Option<String> {
            if !is_atom(atom) {
                return None;
            }
            self.atoms.name(atom_val(atom)).map(str::to_string)
        }
    }

    #[test]
    fn int_round_trip_through_the_c_abi() {
        let mut host = TestHost {
            heap: Heap::new(256).expect("heap"),
            atoms: AtomTable::new(),
        };
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live for the whole test.
        let term = unsafe { enif_make_int(env.as_ptr(), 5) };
        assert_eq!(t(term), make_small(5));
        let mut out: c_int = 0;
        // SAFETY: `out` is a live `c_int` and `env` is live.
        let ok = unsafe { enif_get_int(env.as_ptr(), term, &mut out) };
        assert_eq!((ok, out), (1, 5));
    }

    #[test]
    fn table_exposes_representative_slots_and_leaves_variadics_none() {
        let table = enif_api_table();
        assert!(table.enif_alloc.is_some());
        assert!(table.enif_make_int.is_some());
        assert!(table.enif_get_tuple.is_some());
        assert!(table.enif_make_badarg.is_some());
        // Variadic: stable Rust cannot define these.
        assert!(table.enif_make_tuple.is_none());
        assert!(table.enif_make_list.is_none());
        assert!(table.enif_set_option.is_none());
        assert!(table.enif_fprintf.is_none());
    }
}
