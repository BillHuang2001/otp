//! `enif_*` — the `enif_make_*` constructors (maps excepted; see [`super::map`]).
//!
//! Owned slots: `enif_make_int`, `enif_make_uint`, `enif_make_long`, `enif_make_ulong`,
//! `enif_make_double`, `enif_make_atom`, `enif_make_atom_len`, `enif_make_new_atom`,
//! `enif_make_new_atom_len`, `enif_make_list_cell`, `enif_make_string`, `enif_make_string_len`,
//! `enif_make_ref`, `enif_make_tuple_from_array`, `enif_make_list_from_array`,
//! `enif_make_reverse_list`.
//!
//! `enif_make_int64`/`enif_make_uint64` are also implemented here, but on LP64 the header `#define`s
//! them onto their `long`/`ulong` siblings, so they are free symbols rather than `ErlNifApi` slots.
//! `enif_make_existing_atom`/`enif_make_existing_atom_len` (non-variadic) belong here too and are
//! still to be implemented.

use super::*;

use libc::{c_char, c_double, c_int, c_long, c_uint, c_ulong, size_t};

use crate::nif::types::ErlNifCharEncoding;

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_make_int = Some(enif_make_int);
    api.enif_make_uint = Some(enif_make_uint);
    api.enif_make_long = Some(enif_make_long);
    api.enif_make_ulong = Some(enif_make_ulong);
    api.enif_make_double = Some(enif_make_double);
    api.enif_make_atom = Some(enif_make_atom);
    api.enif_make_atom_len = Some(enif_make_atom_len);
    api.enif_make_new_atom = Some(enif_make_new_atom);
    api.enif_make_new_atom_len = Some(enif_make_new_atom_len);
    api.enif_make_list_cell = Some(enif_make_list_cell);
    api.enif_make_string = Some(enif_make_string);
    api.enif_make_string_len = Some(enif_make_string_len);
    api.enif_make_ref = Some(enif_make_ref);
    api.enif_make_tuple_from_array = Some(enif_make_tuple_from_array);
    api.enif_make_list_from_array = Some(enif_make_list_from_array);
    api.enif_make_reverse_list = Some(enif_make_reverse_list);
    api
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
