#![cfg(beam_abi_variadic_shim)]
//! End-to-end proof of the seven **C-variadic** `ErlNifApi` slots, exercised from a **real C NIF**.
//!
//! `tests/fixtures/nif_variadic.c` is an ordinary NIF module compiled with the system `cc` against
//! the system `erl_nif.h`. It calls `enif_make_tuple`, `enif_make_list`, `enif_set_option`,
//! `enif_fprintf`, `enif_snprintf`, `enif_vfprintf` and `enif_vsnprintf` — the seven slots stable
//! Rust cannot define itself (`rust-lang/rust#44930`). Their bodies live in
//! `csrc/nif_variadic.c` (which owns the `va_list` handling) and forward into
//! `beam_abi::nif::enif::variadic`'s `#[no_mangle]` Rust helpers. The library is `dlopen`ed
//! (`RTLD_NOW`) through [`load`], so a *single* unresolved `enif_*` import would make the load fail:
//! a successful load is itself the proof that the shim's symbols are linked into, and exported
//! from, the test binary.
//!
//! # Why this test is gated
//!
//! The whole file is `#[cfg(beam_abi_variadic_shim)]`: `build.rs` sets that cfg only when it
//! managed to compile the shim with `$CC`/`cc`. Without a C compiler the build still succeeds, the
//! seven slots stay `None`, and this file compiles to an empty (zero-test) binary rather than
//! failing. Independently, [`common::compile_so`] prints a loud `SKIP:` banner and returns `None`
//! when the compiler or the OTP include directory disappears between the build and the test run —
//! every test then skips instead of failing.
mod common;

use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::NifEnv;
use beam_term::prelude::{self, car, cdr, is_list, is_small, signed_value, Eterm, NIL};
use common::{compile_so, skip, TestHost};

/// The committed C fixture — a real NIF module that calls all seven C-variadic `enif_*` functions.
const FIXTURE_C: &str = include_str!("fixtures/nif_variadic.c");

/// The module name `ERL_NIF_INIT` stamps into the entry (`ErlNifEntry.name`).
const MODULE: &str = "nif_variadic";

// ---------------------------------------------------------------------------------------------
// Driving the fixture through the crate's own `dlopen` NIF loader
// ---------------------------------------------------------------------------------------------

/// Compile + load the fixture, then make one call.
///
/// Returns `None` after a loud skip when the toolchain is unavailable, and panics with a readable
/// message when the library fails to load (a load failure here is *always* a shim problem, never an
/// environment problem — the environment was checked first).
fn call(name: &str, arity: u32, args: &[Eterm]) -> Option<Eterm> {
    let so = compile_so(MODULE, FIXTURE_C)?;
    let Some(path) = so.to_str().map(str::to_string) else {
        skip("compiled .so path is not valid UTF-8");
        return None;
    };
    let mut host = TestHost::new();
    // The host must outlive the library (its `unload` callback borrows it), so `host` is declared
    // before `library` and dropped after it.
    let library: NifLibrary = load(&path, MODULE, &mut host, Eterm::nil())
        .unwrap_or_else(|err| panic!("the variadic fixture must load ({err:?})"));
    let mut env = NifEnv::new(&mut host);
    match library.call(&mut env, name, arity, args) {
        Ok(term) => Some(term),
        Err(err) => panic!("{MODULE}:{name}/{arity} must return a term ({err:?})"),
    }
}

/// Read a proper list of small integers back into bytes.
fn bytes(term: Eterm) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut cursor = term;
    loop {
        if cursor == NIL {
            return Some(out);
        }
        if !is_list(cursor) {
            return None;
        }
        // SAFETY: `cursor` is a live cons cell (checked by `is_list` on entry and every step).
        let (head, tail) = unsafe { (car(cursor), cdr(cursor)) };
        if !is_small(head) {
            return None;
        }
        let value = signed_value(head);
        if !(0..=255).contains(&value) {
            return None;
        }
        out.push(value as u8);
        cursor = tail;
    }
}

/// Split the fixture's `{ByteList, Ret}` result into `(text, ret)`.
fn pair(term: Eterm) -> (String, i64) {
    // SAFETY: the fixture always returns a 2-tuple (see `result_pair` in the fixture).
    assert!(
        unsafe { prelude::is_tuple(term) },
        "the fixture returns a tuple"
    );
    // SAFETY: `term` is a live tuple (just tested).
    assert_eq!(unsafe { prelude::tuple_size(term) }, 2);
    // SAFETY: both indices are < the arity of a live tuple.
    let (list, ret) = unsafe { (prelude::tuple_element(term, 0), prelude::tuple_element(term, 1)) };
    let text = bytes(list).expect("the first element is a byte list");
    assert!(is_small(ret), "the second element is the return code");
    (
        String::from_utf8(text).expect("ASCII output"),
        signed_value(ret),
    )
}

/// The three small ints `1, 2, 3` as immediates (no heap needed).
fn smalls() -> [Eterm; 3] {
    [
        prelude::make_small(1),
        prelude::make_small(2),
        prelude::make_small(3),
    ]
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------

/// `enif_make_tuple(env, 3, a, b, c)` builds a tuple of arity 3 in **declared** order.
#[test]
fn variadic_make_tuple_preserves_arity_and_order() {
    let args = smalls();
    let Some(term) = call("tuple3", 3, &args) else {
        return;
    };
    // SAFETY: `tuple3` returns a tuple (the fixture calls `enif_make_tuple`).
    assert!(unsafe { prelude::is_tuple(term) });
    // SAFETY: `term` is a live tuple.
    assert_eq!(unsafe { prelude::tuple_size(term) }, 3);
    for (index, expected) in [1i64, 2, 3].into_iter().enumerate() {
        // SAFETY: `index < 3 == arity` for a live tuple.
        let element = unsafe { prelude::tuple_element(term, index) };
        assert!(is_small(element));
        assert_eq!(signed_value(element), expected, "element {index}");
    }
}

/// `enif_make_list(env, 3, a, b, c)` builds the proper list `[a, b, c]` in **declared** order
/// (not reversed, which is the classic way to get a variadic list builder wrong).
#[test]
fn variadic_make_list_preserves_order() {
    let args = smalls();
    let Some(term) = call("list3", 3, &args) else {
        return;
    };
    assert_eq!(bytes(term), Some(vec![1, 2, 3]));
}

/// The ERTS `*printf` subset: `%d`, `%s`, `%#x` and the ERTS-specific `%p`.
///
/// `%p` is the discriminator: ERTS renders a null pointer as `0x` followed by `2*sizeof(void*)`
/// digits, where libc would print `(nil)`.
#[test]
fn snprintf_matches_the_erts_format_subset() {
    let Some(term) = call("snprintf_mixed", 0, &[]) else {
        return;
    };
    let (text, ret) = pair(term);
    assert_eq!(text, "a=-7 s=hi x=0xff p=0x0000000000000000");
    assert_eq!(ret, text.len() as i64, "the return value is the length");
}

/// A short buffer truncates the text but **not** the return value: `erts_vsnprintf` returns what
/// the formatter produced, not what `write_sn` copied.
#[test]
fn snprintf_truncates_but_returns_the_full_length() {
    let Some(term) = call("snprintf_trunc", 0, &[]) else {
        return;
    };
    let (text, ret) = pair(term);
    assert_eq!(text, "abc", "a 4-byte buffer holds 3 bytes plus the NUL");
    assert_eq!(ret, 6, "the return value is the full rendered length");
}

/// `enif_vsnprintf` — the `va_list` form — behaves exactly like `enif_snprintf`.
#[test]
fn vsnprintf_matches_the_variadic_form() {
    let Some(term) = call("vsnprintf_len", 0, &[]) else {
        return;
    };
    let (text, ret) = pair(term);
    assert_eq!(text, "file-42");
    assert_eq!(ret, 7);
}

/// `enif_fprintf` really writes to the stream it is handed.
#[test]
fn fprintf_writes_to_the_stream() {
    let Some(term) = call("fprintf_file", 0, &[]) else {
        return;
    };
    let (text, ret) = pair(term);
    assert_eq!(text, "file-42");
    assert_eq!(ret, 7);
}

/// `enif_vfprintf` — the `va_list` stream form.
#[test]
fn vfprintf_writes_to_the_stream() {
    let Some(term) = call("vfprintf_file", 0, &[]) else {
        return;
    };
    let (text, ret) = pair(term);
    assert_eq!(text, "ab");
    assert_eq!(ret, 2);
}

/// `enif_set_option` answers `EINVAL` for every option on this runtime — the oracle's own answer
/// when the env carries no loading module instance (`erl_nif.c:5517-5580`), not a stub.
#[test]
fn set_option_is_einval_without_a_loading_module_instance() {
    for option in 1..=3 {
        let args = [prelude::make_small(option)];
        let Some(term) = call("set_option", 1, &args) else {
            return;
        };
        assert!(is_small(term));
        assert_eq!(
            signed_value(term),
            libc::EINVAL as i64,
            "option {option} must be refused"
        );
    }
}
