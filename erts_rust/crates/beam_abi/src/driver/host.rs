//! The driver host seam — the runtime services the `erl_drv_*` layer needs.
//!
//! `beam_abi` freezes the *shape* of the driver C ABI (the `ErlDrv*` types and the
//! `driver_*`/`erl_drv_*` table in [`super::api`]). The driver functions cannot do their job alone:
//! `erl_drv_output_term`/`erl_drv_send_term` must turn a **term-data array** into real terms
//! (allocating on a heap, interning atoms), `driver_select` must register an event with the poller,
//! and `driver_set_timer` must talk to the timer wheel. Those are **runtime** services — `beam_abi`
//! must **not** pull in the scheduler, `beam_io` or a specific module's atom table.
//!
//! [`DriverHost`] is that seam. [`DriverEnv`] is the concrete per-port environment behind the
//! C-facing opaque `*mut ErlDrvPort_` (`struct _erl_drv_port`, opaque in [`super::types`]).
//!
//! **The `ErlDrvPort` a runtime passes to a driver callback IS the pointer returned by
//! [`DriverEnv::as_port`]**, recovered with [`DriverEnv::from_port`] — the exact analogue of
//! [`NifEnv::as_ptr`](crate::nif::host::NifEnv::as_ptr) /
//! [`NifEnv::from_opaque`](crate::nif::host::NifEnv::from_opaque). `unsafe` is confined to
//! [`DriverEnv::host`], [`DriverEnv::from_port`] and [`decode_term_spec`]'s operand readers, each
//! documented with a `# Safety` section / `// SAFETY:` rationale (workspace `CONTEXT.md` §3).

use libc::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

use beam_alloc::Heap;
use beam_term::prelude::{self, Eterm, NIL};

use crate::driver::types::{
    ErlDrvBinary, ErlDrvEvent, ErlDrvPort, ErlDrvPort_, ErlDrvTermData, ERL_DRV_ATOM,
    ERL_DRV_BINARY, ERL_DRV_BUF2BINARY, ERL_DRV_FLOAT, ERL_DRV_INT, ERL_DRV_INT64, ERL_DRV_LIST,
    ERL_DRV_NIL, ERL_DRV_PORT, ERL_DRV_STRING, ERL_DRV_STRING_CONS, ERL_DRV_TUPLE, ERL_DRV_UINT,
    ERL_DRV_UINT64,
};

/// Services the runtime provides to the `erl_drv_*` implementations of one port.
pub trait DriverHost {
    /// The term-allocation arena for the duration of the current call. Must not reallocate while the
    /// driver holds a pointer into it (see [`beam_alloc::Heap`]).
    fn heap(&mut self) -> &mut Heap;

    /// The runtime's global atom table: intern `name`, returning the atom term.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm>;

    /// Deliver `data` to the port owner (`driver_output`/`erl_drv_output`). `0` = ok, `-1` = fail.
    fn output(&mut self, port: ErlDrvPort, data: &[u8]) -> c_int;

    /// Deliver a decoded term (`erl_drv_output_term`/`erl_drv_send_term`): to the port owner when
    /// `to` is `None`, else to that specific process. `0` = ok, `-1` = fail.
    fn send_term(&mut self, port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int;

    /// Register/cancel interest in `event` (`driver_select`). `mode` is `ERL_DRV_READ`/
    /// `ERL_DRV_WRITE`/`ERL_DRV_USE`; `on` non-zero selects, zero deselects. `0` = ok, `-1` = fail
    /// (default: unsupported).
    fn select(&mut self, port: ErlDrvPort, event: ErlDrvEvent, mode: c_int, on: c_int) -> c_int {
        let _ = (port, event, mode, on);
        -1
    }

    /// Start a `ms`-millisecond timer for `port` (`driver_set_timer`). `0` = ok, `-1` = fail
    /// (default: unsupported).
    fn set_timer(&mut self, port: ErlDrvPort, ms: c_ulong) -> c_int {
        let _ = (port, ms);
        -1
    }

    /// Cancel `port`'s timer (`driver_cancel_timer`). `0` = ok, `-1` = fail (default: unsupported).
    fn cancel_timer(&mut self, port: ErlDrvPort) -> c_int {
        let _ = port;
        -1
    }

    /// The remaining milliseconds of `port`'s timer (`driver_read_timer`), or `None` when there is
    /// no timer / the host does not support them (default: unsupported).
    fn read_timer(&mut self, port: ErlDrvPort) -> Option<c_ulong> {
        let _ = port;
        None
    }

    /// Schedule an asynchronous `invoke` on `port` (`driver_async`). `key` identifies the job,
    /// `data` is passed to `invoke`, and `free` releases `data` when no `invoke` runs. Returns a
    /// queue position (`=> 0`) or a negative error (default: unsupported, `-1`).
    ///
    /// A raw identifier because `async` is a Rust keyword; the C name is `driver_async`.
    fn r#async(
        &mut self,
        port: ErlDrvPort,
        key: *mut c_uint,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_long {
        let _ = (port, key, invoke, data, free);
        -1
    }

    /// The pid the port is connected to (`driver_connected`), or `None` (default: unsupported).
    fn connected_pid(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        let _ = port;
        None
    }

    /// The pid that opened the port (`driver_caller`), or `None` (default: unsupported).
    fn caller_pid(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        let _ = port;
        None
    }
}

/// The concrete per-port driver environment behind the C-facing opaque `*mut ErlDrvPort_`.
///
/// Holds a raw pointer to the host, so the environment itself is lifetime-free and can be recovered
/// from the opaque C pointer by [`DriverEnv::from_port`] (see the module docs).
pub struct DriverEnv {
    /// The host, borrowed for the lifetime of this environment.
    ///
    /// A raw fat pointer rather than `&'a mut dyn DriverHost`: the C side only ever sees
    /// `*mut ErlDrvPort_`, and the recover path ([`DriverEnv::from_port`]) has no way to name the
    /// borrow's lifetime. The caller must keep the host alive for as long as the environment is.
    /// (The `'static` that a `*mut dyn Trait` spells by default is erased by the documented
    /// lifetime-erasing cast in [`DriverEnv::new`].)
    host: *mut dyn DriverHost,
}

impl DriverEnv {
    /// Wrap `host` in a fresh per-port environment.
    pub fn new(host: &mut dyn DriverHost) -> DriverEnv {
        // SAFETY: the only thing reinterpreted here is the erased trait-object lifetime. The
        // C-facing env has no lifetime parameter (`struct _erl_drv_port` is plain C), so the
        // borrow cannot be expressed in the type; `host as *mut dyn DriverHost` alone would be
        // rejected because a raw pointer to a trait object is invariant and defaults to `'static`
        // (raw pointer *casts* may not extend a trait-object lifetime either — rust#141402).
        // `&mut dyn DriverHost` and `*mut dyn DriverHost` are layout-identical (data word + vtable
        // word), and `mem::transmute` between them is a bit-preserving cast, so the caller's borrow
        // is reproduced exactly by `DriverEnv::host`. The caller must keep the host alive for as
        // long as the environment is used (`DriverEnv` is not `Clone`, so there is exactly one
        // handle).
        let host: *mut dyn DriverHost = unsafe { core::mem::transmute(host) };
        DriverEnv { host }
    }

    /// Reborrow the host (the env borrows the host for its lifetime).
    pub fn host(&mut self) -> &mut dyn DriverHost {
        // SAFETY: `host` was produced from a live `&mut dyn DriverHost` in `new` (or a previous
        // `host`/`as_port`/`from_port` round-trip), and `DriverEnv` is not `Clone`, so this is the
        // only outstanding handle to it; the returned reborrow is tied to `&mut self`.
        unsafe { &mut *self.host }
    }

    /// Convenience: `self.host().heap()`.
    pub fn heap(&mut self) -> &mut Heap {
        self.host().heap()
    }

    /// The C-facing opaque port pointer. Casts `self as *mut DriverEnv as *mut ErlDrvPort_`.
    pub fn as_port(&mut self) -> ErlDrvPort {
        self as *mut DriverEnv as *mut ErlDrvPort_
    }

    /// Recover the concrete env from an opaque port pointer.
    ///
    /// # Safety
    ///
    /// `port` must be a pointer previously returned by [`DriverEnv::as_port`] for an env that is
    /// still alive (and whose host is still alive). Returns `None` for a null pointer.
    pub unsafe fn from_port<'a>(port: ErlDrvPort) -> Option<&'a mut DriverEnv> {
        if port.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees `port` came from `DriverEnv::as_port` on a live env, i.e. it
        // is a `*mut DriverEnv` with the provenance required to reborrow the same allocation; `'a`
        // is the caller's promise that the env outlives the returned reference.
        Some(unsafe { &mut *(port as *mut DriverEnv) })
    }
}

// The term-data array decoder (`erl_drv_output_term`/`erl_drv_send_term`).

/// Turn a driver **term-data array** into an [`Eterm`], building the term on the host heap.
///
/// The array is the C `ErlDrvTermData spec[]`: tag/operand pairs in *reverse Polish* order —
/// operands are pushed first, and a `TUPLE`/`LIST` tag pops its operands (ground truth
/// `erts/emulator/beam/io.c`, `ERTS_DDT_*`). It must leave **exactly one** value on the stack and be
/// fully consumed, as the C requires (`io.c`: `if (depth != 1 || ptr != ptr_end) fail`). Returns
/// `None` for any malformed spec — never a wrong term.
///
/// Handled tags: `ERL_DRV_NIL`, `ERL_DRV_ATOM`, `ERL_DRV_INT`, `ERL_DRV_UINT`, `ERL_DRV_INT64`,
/// `ERL_DRV_UINT64`, `ERL_DRV_STRING`, `ERL_DRV_STRING_CONS`, `ERL_DRV_BUF2BINARY`,
/// `ERL_DRV_BINARY`, `ERL_DRV_TUPLE`, `ERL_DRV_LIST`, `ERL_DRV_PORT`, `ERL_DRV_FLOAT`.
///
/// # Documented deviations from the C (never a wrong term)
///
/// * `ERL_DRV_ATOM`'s operand is treated as a NUL-terminated `char*` interned through
///   [`DriverHost::intern_atom`]. In the C the operand is already an atom term from
///   `driver_mk_atom`; this seam has no `driver_mk_atom`, so the spec carries the *name* and the
///   host interns it — the same atom either way.
/// * `ERL_DRV_PID`, `ERL_DRV_EXT2TERM` and `ERL_DRV_MAP` are **unsupported** (`None`): they need a
///   pid term, an ETF decoder and a map builder outside this seam.
/// * Counts (`LIST`/`TUPLE` arity, `STRING`/`BUF2BINARY` length) are read as non-negative 32-bit
///   values, mirroring the C's `(int)` casts, so a malformed spec cannot request an astronomically
///   large copy. `ERL_DRV_BINARY`'s `size`/`offset` are read as full words and bounds-checked
///   against the binary's `orig_size`, exactly as the C does.
pub fn decode_term_spec(env: &mut DriverEnv, spec: &[ErlDrvTermData]) -> Option<Eterm> {
    // The intermediate value stack (the C's `ESTACK_PUSH`/`ESTACK_POP`).
    let mut stack: Vec<Eterm> = Vec::new();
    let mut ix = 0usize;

    while ix < spec.len() {
        let tag = spec[ix];
        ix += 1;
        match tag {
            ERL_DRV_NIL => stack.push(NIL),

            ERL_DRV_ATOM => {
                let name = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_ATOM` operand is a NUL-terminated C string (see the docs).
                let bytes = unsafe { cstr(name as *const c_char) }?;
                stack.push(env.host().intern_atom(core::str::from_utf8(bytes).ok()?)?);
            }

            ERL_DRV_INT => {
                let value = *spec.get(ix)?;
                ix += 1;
                stack.push(int_from_i64(env, value as i64)?);
            }

            ERL_DRV_UINT => {
                let value = *spec.get(ix)?;
                ix += 1;
                stack.push(int_from_u64(env, value)?);
            }

            ERL_DRV_INT64 => {
                let ptr = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_INT64` operand is a `Sint64*`.
                let value = unsafe { read_scalar(ptr as *const i64) }?;
                stack.push(int_from_i64(env, value)?);
            }

            ERL_DRV_UINT64 => {
                let ptr = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_UINT64` operand is a `Uint64*`.
                let value = unsafe { read_scalar(ptr as *const u64) }?;
                stack.push(int_from_u64(env, value)?);
            }

            ERL_DRV_STRING | ERL_DRV_STRING_CONS => {
                let ptr = *spec.get(ix)?;
                let len = *spec.get(ix + 1)?;
                ix += 2;
                let n = count_operand(len)?;
                if ptr == 0 {
                    return None; // The C rejects a NULL buffer even for a zero length.
                }
                let tail = if tag == ERL_DRV_STRING_CONS {
                    stack.pop()? // The C requires a value to cons onto (`depth < 1` fails).
                } else {
                    NIL
                };
                // SAFETY: `(char*, length)` declares `n` readable bytes at `ptr`.
                let bytes = unsafe { buf(ptr as *const c_char, n) }?;
                stack.push(byte_list(env, bytes, tail)?);
            }

            ERL_DRV_BUF2BINARY => {
                let ptr = *spec.get(ix)?;
                let len = *spec.get(ix + 1)?;
                ix += 2;
                let n = count_operand(len)?;
                // SAFETY: as `ERL_DRV_STRING`; the C allows a NULL only when the length is 0.
                let bytes = unsafe { buf(ptr as *const c_char, n) }?;
                stack.push(binary_term(env, bytes)?);
            }

            ERL_DRV_BINARY => {
                let bin = *spec.get(ix)?;
                let size = *spec.get(ix + 1)?;
                let offset = *spec.get(ix + 2)?;
                ix += 3;
                // SAFETY: the operand is an `ErlDrvBinary*`; `driver_binary_bytes` bounds-checks
                // `(offset, size)` against `orig_size` before reading.
                let bytes =
                    unsafe { driver_binary_bytes(bin as *const ErlDrvBinary, size, offset) }?;
                stack.push(binary_term(env, bytes)?);
            }

            ERL_DRV_TUPLE => {
                let n = count_operand(*spec.get(ix)?)?;
                ix += 1;
                if n == 0 {
                    stack.push(prelude::tuple0()); // The C's shared empty-tuple literal.
                    continue;
                }
                if stack.len() < n {
                    return None;
                }
                let elems = stack.split_off(stack.len() - n);
                stack.push(env.heap().tuple(&elems).ok()?);
            }

            ERL_DRV_LIST => {
                let n = count_operand(*spec.get(ix)?)?;
                ix += 1;
                if n == 0 || stack.len() < n {
                    return None; // The C rejects `ERL_DRV_LIST 0` outright (`ptr[0] <= 0`).
                }
                let elems = stack.split_off(stack.len() - n);
                let mut acc = NIL;
                for elem in elems.into_iter().rev() {
                    acc = env.heap().cons(elem, acc).ok()?;
                }
                stack.push(acc);
            }

            ERL_DRV_PORT => {
                let term = Eterm::from_raw(*spec.get(ix)? as usize);
                ix += 1;
                if !prelude::is_internal_port(term) {
                    return None; // The C rejects a non-internal-port operand.
                }
                stack.push(term);
            }

            ERL_DRV_FLOAT => {
                // SAFETY: the `ERL_DRV_FLOAT` operand is a `double*`.
                let value = unsafe { read_scalar(*spec.get(ix)? as *const f64) }?;
                ix += 1;
                stack.push(float_term(env, value)?);
            }

            // `ERL_DRV_PID` / `ERL_DRV_EXT2TERM` / `ERL_DRV_MAP` and any unknown tag.
            _ => return None,
        }
    }

    // Exactly one value, array fully consumed (the C's final depth check).
    if stack.len() != 1 {
        return None;
    }
    stack.pop()
}

// Operand readers and term builders.

/// A count operand (`LIST`/`TUPLE` arity, `STRING`/`BUF2BINARY` length) as a `usize`.
///
/// The C reads these through an `(int)` cast and rejects a negative result, so a word whose low 32
/// bits are negative is malformed rather than an enormous count.
fn count_operand(word: ErlDrvTermData) -> Option<usize> {
    let n = word as i32;
    if n < 0 {
        None
    } else {
        Some(n as usize)
    }
}

/// A borrowed `&[u8]` view over the `len` bytes at `ptr`.
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes (a null `ptr` is accepted only for `len == 0`).
unsafe fn buf<'a>(ptr: *const c_char, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return if len == 0 { Some(&[]) } else { None };
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

/// A borrowed `&[u8]` view over a NUL-terminated C string (`None` when the pointer is null).
///
/// # Safety
///
/// `ptr` must be null or point at a NUL-terminated byte sequence valid for the call.
unsafe fn cstr<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string; `CStr::from_ptr` only reads up to it.
    Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes())
}

/// Read one `Copy` value through `ptr` (`None` when the pointer is null).
///
/// # Safety
///
/// `ptr` must be null or point at one readable, properly-aligned value of type `T`.
unsafe fn read_scalar<T: Copy>(ptr: *const T) -> Option<T> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees one readable, aligned `T` at `ptr`.
    Some(unsafe { ptr.read() })
}

/// The `size` bytes at byte `offset` inside an `ErlDrvBinary` (`None` when malformed).
///
/// # Safety
///
/// `bin` must be null or point at a live `ErlDrvBinary` whose `orig_bytes` flexible array holds at
/// least `orig_size` bytes.
unsafe fn driver_binary_bytes<'a>(
    bin: *const ErlDrvBinary,
    size: ErlDrvTermData,
    offset: ErlDrvTermData,
) -> Option<&'a [u8]> {
    if bin.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live `ErlDrvBinary` at `bin`.
    let orig_size = unsafe { (*bin).orig_size };
    if orig_size < 0 {
        return None;
    }
    // The C rejects `size + offset > b->orig_size` (an overflow-free checked add here).
    if offset.checked_add(size)? > orig_size as u64 {
        return None;
    }
    if size == 0 {
        return Some(&[]);
    }
    // SAFETY: the caller guarantees `orig_size` readable bytes at `orig_bytes`; `offset + size` is
    // within `orig_size` (checked above), so `base.add(offset)` heads `size` readable bytes.
    let base = unsafe { core::ptr::addr_of!((*bin).orig_bytes) } as *const u8;
    Some(unsafe { core::slice::from_raw_parts(base.add(offset as usize), size as usize) })
}

/// A list of small ints from `bytes` (unsigned, as the C's `buf_to_intlist`), ending in `tail`.
fn byte_list(env: &mut DriverEnv, bytes: &[u8], tail: Eterm) -> Option<Eterm> {
    let mut acc = tail;
    for &byte in bytes.iter().rev() {
        acc = env.heap().cons(prelude::make_small(byte as i64), acc).ok()?;
    }
    Some(acc)
}

/// An on-heap byte-aligned binary term from `bytes` (`None` on heap exhaustion).
fn binary_term(env: &mut DriverEnv, bytes: &[u8]) -> Option<Eterm> {
    let bits = bytes.len().checked_mul(8)?;
    // SAFETY: `heap_bits_size_words(n_bits)` sizes the `ErlHeapBits` block for `n_bits` bits.
    let hp = unsafe { env.heap().alloc_words(prelude::heap_bits_size_words(bits)) }.ok()?;
    // SAFETY: `hp` heads the reserved block and `bytes` provides the payload.
    unsafe { prelude::write_binary(hp, bytes) }.ok()
}

/// A float term from `value` (`None` for a non-finite value, which the C rejects).
fn float_term(env: &mut DriverEnv, value: f64) -> Option<Eterm> {
    if !value.is_finite() {
        return None;
    }
    // SAFETY: `FLOAT_SIZE_WORDS` (2) sizes the `[HEADER_FLONUM][bits]` block.
    let hp = unsafe { env.heap().alloc_words(prelude::FLOAT_SIZE_WORDS) }.ok()?;
    // SAFETY: `hp` heads the reserved 2-word block, both words of which are written.
    Some(unsafe { prelude::write_float(hp, value) })
}

/// `make_small` when the value fits, else a one-limb bignum of the right sign.
///
/// Equivalent to `nif/enif.rs`'s `small_or_big`, which cannot be shared without touching that
/// module (out of this step's scope); it is a pure function of `beam_term`'s public writers.
fn small_or_big(env: &mut DriverEnv, negative: bool, magnitude: u64) -> Option<Eterm> {
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
        return Some(prelude::make_small(value));
    }
    // SAFETY: `big_size_words(1)` sizes the `[hdr][digit]` block.
    let hp = unsafe { env.heap().alloc_words(prelude::big_size_words(1)) }.ok()?;
    // SAFETY: `hp` heads the reserved `1 + 1`-word block, which `write_big` writes in full.
    Some(unsafe { prelude::write_big(hp, negative, &[magnitude]) })
}

/// An `ERL_DRV_INT` operand (`ErlDrvSInt`, a `long` on LP64) as a term.
fn int_from_i64(env: &mut DriverEnv, value: i64) -> Option<Eterm> {
    if value < 0 {
        small_or_big(env, true, value.unsigned_abs())
    } else {
        small_or_big(env, false, value as u64)
    }
}

/// An `ERL_DRV_UINT`/`ERL_DRV_UINT64` operand (`ErlDrvUInt`) as a term.
fn int_from_u64(env: &mut DriverEnv, value: u64) -> Option<Eterm> {
    small_or_big(env, false, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_term::atom::AtomTable;
    use beam_term::prelude::{
        atom_val, car, cdr, is_atom, is_big, is_list, is_small, is_tuple, make_atom, make_small,
        signed_value, tuple_element, tuple_size,
    };
    use std::ffi::CString;

    use crate::driver::types::{term_data, ERL_DRV_PID};

    /// A minimal [`DriverHost`]: a bump heap, an atom table, and recording output/term buffers.
    struct TestDriverHost {
        heap: Heap,
        atoms: AtomTable,
        output: Vec<u8>,
        sent: Vec<Eterm>,
    }

    impl TestDriverHost {
        fn new() -> TestDriverHost {
            TestDriverHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                output: Vec::new(),
                sent: Vec::new(),
            }
        }

        fn atoms(&self) -> &AtomTable {
            &self.atoms
        }
    }

    impl DriverHost for TestDriverHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
            self.output.extend_from_slice(data);
            0
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, term: Eterm) -> c_int {
            self.sent.push(term);
            0
        }
    }

    #[test]
    fn as_port_round_trips_through_from_port() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        assert!(!port.is_null(), "the port pointer is never null");
        // SAFETY: `port` came from `DriverEnv::as_port` on `env`, which is still alive here.
        let recovered = unsafe { DriverEnv::from_port(port) }.expect("non-null port");
        // The recovered env drives the very same host: a term sent through it lands in `host`.
        // (`DriverEnv::new` erases the borrow, so `host` is usable again here.)
        assert_eq!(recovered.host().send_term(port, None, make_small(7)), 0);
        assert_eq!(host.sent, vec![make_small(7)]);
    }

    #[test]
    fn from_port_of_null_is_none() {
        // SAFETY: a null pointer is explicitly allowed by the contract.
        let recovered = unsafe { DriverEnv::from_port(std::ptr::null_mut()) };
        assert!(recovered.is_none());
    }

    #[test]
    fn decode_term_spec_handles_an_atom() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let name = CString::new("hello").expect("no interior NUL");
        let spec = [ERL_DRV_ATOM, term_data(name.as_ptr() as usize)];
        let term = decode_term_spec(&mut env, &spec).expect("a valid atom spec");
        assert!(is_atom(term));
        assert_eq!(host.atoms().name(atom_val(term)).as_deref(), Some("hello"));
    }

    #[test]
    fn decode_term_spec_handles_ints_and_bignums() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, 42]).expect("a valid int spec");
        assert!(is_small(term));
        assert_eq!(signed_value(term), 42);

        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, (-1i64) as ErlDrvTermData])
            .expect("a negative int is valid");
        assert_eq!(signed_value(term), -1);

        // A magnitude outside the 60-bit small range becomes a bignum.
        let term =
            decode_term_spec(&mut env, &[ERL_DRV_UINT, u64::MAX]).expect("a valid uint spec");
        // SAFETY: `term` is a valid boxed term (a bignum, never a small int here).
        assert!(unsafe { is_big(term) });
    }

    #[test]
    fn decode_term_spec_handles_a_string() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let text = b"abc";
        let spec = [
            ERL_DRV_STRING,
            term_data(text.as_ptr() as usize),
            text.len() as ErlDrvTermData,
        ];
        let term = decode_term_spec(&mut env, &spec).expect("a valid string spec");
        // "abc" is the list [97, 98, 99].
        assert!(is_list(term));
        // SAFETY: `term` is a live three-element list.
        let (a, rest) = unsafe { (car(term), cdr(term)) };
        // SAFETY: `rest` is a live two-element list.
        let (b, rest) = unsafe { (car(rest), cdr(rest)) };
        // SAFETY: `rest` is a live one-element list.
        let (c, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!(
            [signed_value(a), signed_value(b), signed_value(c)],
            [97, 98, 99]
        );
        assert_eq!(tail, NIL);
    }

    #[test]
    fn decode_term_spec_handles_a_tuple_and_a_list() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let name = CString::new("a").expect("no interior NUL");

        // {a, 1}
        let spec = [
            ERL_DRV_ATOM,
            term_data(name.as_ptr() as usize),
            ERL_DRV_INT,
            1,
            ERL_DRV_TUPLE,
            2,
        ];
        let term = decode_term_spec(&mut env, &spec).expect("a valid tuple spec");
        // SAFETY: `term` is a live tuple (asserted by the size check below).
        assert!(unsafe { is_tuple(term) });
        // SAFETY: as above.
        assert_eq!(unsafe { tuple_size(term) }, 2);
        // SAFETY: element 0 of a 2-tuple.
        assert!(is_atom(unsafe { tuple_element(term, 0) }));
        // SAFETY: element 1 of a 2-tuple.
        assert_eq!(signed_value(unsafe { tuple_element(term, 1) }), 1);

        // [1, 2]
        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, 1, ERL_DRV_INT, 2, ERL_DRV_LIST, 2])
            .expect("a valid list spec");
        assert!(is_list(term));
        // SAFETY: `term` is a live two-element list.
        let (head, rest) = unsafe { (car(term), cdr(term)) };
        // SAFETY: `rest` is a live one-element list.
        let (second, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!(signed_value(head), 1);
        assert_eq!(signed_value(second), 2);
        assert_eq!(tail, NIL);
    }

    #[test]
    fn decode_term_spec_rejects_malformed_and_unsupported_specs() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        // An empty spec leaves no value; a tag whose operands are missing; two values left on the
        // stack; a tuple arity larger than the stack; `ERL_DRV_LIST 0` (which the C rejects);
        // `ERL_DRV_PID` (like `EXT2TERM`/`MAP`, unsupported); a NULL string buffer; a null float
        // pointer.
        for spec in [
            &[] as &[ErlDrvTermData],
            &[ERL_DRV_INT],
            &[ERL_DRV_INT, 1, ERL_DRV_INT, 2],
            &[ERL_DRV_TUPLE, 1],
            &[ERL_DRV_LIST, 0],
            &[ERL_DRV_PID, 0],
            &[ERL_DRV_STRING, 0, 0],
            &[ERL_DRV_FLOAT, 0],
        ] {
            assert_eq!(decode_term_spec(&mut env, spec), None, "spec {spec:?}");
        }
    }
}
