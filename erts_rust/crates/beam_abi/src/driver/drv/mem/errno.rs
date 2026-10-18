// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%
//! The **errno subsystem** of the driver host layer's host-free half — `erl_errno_id/1` and the
//! permanent answer storage behind it (workspace contract **A.4**).
//!
//! `char *erl_errno_id(int error)` answers an errno's **lowercase POSIX name** (`eacces`, `enoent`,
//! `eio`) or the `errno_<N>` form, exactly as `erts/emulator/beam/erl_errno_str.c` does, and the
//! answer must never move or be freed for the runtime's whole lifetime — the C's own contract:
//! *"Returned strings must not move or be deallocated during the runtime systems lifetime"*
//! (`:1234-1237`), because a driver may cache several answers taken at different calls. This module
//! is that C file's Rust counterpart:
//!
//! * [`colliding_errno_name`] — the C's `errno_name()` switch (`:979-1108`): the spelling ERTS has
//!   always answered for a value that carries more than one name (`eagain`, `edeadlk`, `ebusy`, …),
//!   consulted *before* any C-library lookup.
//! * [`errno_table::errno_name_fallback`] — the C's built-in `errno_name_fallback` table
//!   (`:395-977`), which the C compiles in when the C library has no `strerrorname_np` and which
//!   answers here too, so a missing symbol costs no fidelity. It agrees with this platform's
//!   `strerrorname_np` on every value it names (its own cross-check test pins that).
//! * [`ERRNO_ANSWERS`] — the permanent store for anything **rendered** (a C-library name
//!   lowercased, or `errno_<N>`), the analogue of the C's `errno_map` plus its never-pruned hash
//!   table.
//!
//! The module is a child of [`super`] (`mem`): the answer store, the ported table and the lookup
//! order are one subsystem, and `mem` re-exports `erl_errno_id` so `drv::erl_errno_id` keeps naming
//! it.
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};
use libc::{c_char, c_int, c_void};
use super::errno_table;

/// `ERTS_UNKNOWN_ERRNO_BUF_SZ` (`erl_errno_str.c:48`) — the size a rendered answer needs: "big
/// enough even if errno should be 64-bit integer". It holds the `errno_<N>` form (at most `errno_` +
/// `-` + ten digits = 17 bytes), the longest name a C library can report here (`ENOTRECOVERABLE`, 15
/// bytes) and the NUL.
const ERTS_UNKNOWN_ERRNO_BUF_SZ: usize = 30;


/// The **permanent** answer store — this crate's `errno_map` plus never-pruned hash table.
///
/// The C keeps every answer it has ever produced for the runtime's whole lifetime: `errno_map`
/// (`erl_errno_str.c:1152-1175`) is preallocated for `0..=max_errno`, everything beyond it lives in a
/// hash table whose free function is a deliberate no-op ("We *never* remove any elements that has
/// been inserted...", `:165-170`), and the answers are copied into `permanent_alloc`'d memory. That is
/// because of the entry point's own contract: *"Returned strings must not move or be deallocated
/// during the runtime systems lifetime"* (`erl_errno_id`, `:1234-1237`) — a driver may cache several
/// answers from different calls and they all have to stay valid.
///
/// This is that store for the answers this crate has to **render** (a `strerrorname_np` name
/// lowercased, or the `errno_<N>` form). Each value is boxed and leaked **once per distinct errno**
/// (the C's `permanent_alloc` semantics, not per call), so the pointer handed to C never moves and is
/// never freed; a repeat request for a value already stored is a read-lock lookup that allocates
/// nothing. The built-in tables ([`colliding_errno_name`], [`errno_table::errno_name_fallback`]) need
/// no store at all — their names are static literals — so the common errno never even takes the lock.
static ERRNO_ANSWERS: RwLock<BTreeMap<c_int, &'static [u8]>> = RwLock::new(BTreeMap::new());

/// `const char *strerrorname_np(int)` — the C library's errno-number → errno-*name* lookup, the
/// primitive the C's `errno_name()` falls back on under `HAVE_STRERRORNAME_NP`
/// (`erl_errno_str.c:43-45`, `:1106-1108`).
type StrerrorNameNp = unsafe extern "C" fn(c_int) -> *const c_char;

/// The one-time `dlsym` resolution of [`StrerrorNameNp`]. `None` when this C library exports no such
/// function — musl, or glibc older than 2.32 — which is exactly the build where the C compiles its
/// own `errno_name_fallback` in and **never** returns `NULL` for a value that table names
/// (`erl_errno_str.c:395-397`, `:1109-1115`). [`errno_table::errno_name_fallback`] is that table, and
/// [`errno_answer`] consults it before this probe, so a missing symbol costs no fidelity: only a value
/// no table names (and that this library cannot name either) takes the `errno_<N>` form.
static STRERRORNAME_NP: OnceLock<Option<StrerrorNameNp>> = OnceLock::new();

/// Resolve `strerrorname_np` from the process's dynamic symbol table (cached for the process's
/// lifetime, so the hot path is a load plus a null check).
///
/// Safe to call: the `dlsym` probe has no preconditions (it is given `RTLD_DEFAULT` and a
/// NUL-terminated literal, and a symbol found under this name has the C-declared `const char *(int)`
/// signature), and the result is either `None` or a function pointer the caller only invokes with an
/// `int`.
pub(super) fn strerror_name_np() -> Option<StrerrorNameNp> {
    *STRERRORNAME_NP.get_or_init(|| {
        // SAFETY: `RTLD_DEFAULT` is this platform's "global scope" pseudo-handle and the name is a
        // NUL-terminated literal, so `dlsym` reads only its own arguments.
        let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"strerrorname_np".as_ptr()) };
        if sym.is_null() {
            return None;
        }
        // SAFETY: a global symbol named `strerrorname_np` *is* `const char *(*)(int)` (the glibc
        // declaration the C's configure check probes for), and a `dlsym` result is by contract
        // convertible to the type the caller knows the symbol to have. Both are one word on LP64.
        Some(unsafe { core::mem::transmute::<*mut c_void, StrerrorNameNp>(sym) })
    })
}

/// The answer for an errno value that carries more than one name — the C has always returned the
/// same spelling for such a collision, so a fixed table bypasses the C library's possibly-different
/// choice (`erl_errno_str.c:979-1108`): the switch is consulted *before* `strerrorname_np`, and each
/// of its guards is "only list this name if the two spellings are distinct values".
///
/// On this target (`EAGAIN == EWOULDBLOCK`, `EBUSY != EALREADY`, `EWOULDBLOCK != EDEADLK`,
/// `ENOTSUP == EOPNOTSUPP`) that leaves `EAGAIN`, `EALREADY`, `EBUSY`, `ECONNREFUSED`, `EDEADLK` and
/// `ENOTSUP`, in the C's order.
pub(super) fn colliding_errno_name(error: c_int) -> Option<&'static [u8]> {
    // Written as `if`s rather than a `match`: `EAGAIN` and `EWOULDBLOCK` are the same value here, so
    // two arms could never both be reachable and one would be flagged as an unreachable pattern.
    if error == libc::EAGAIN {
        return Some(b"eagain\0");
    }
    if libc::EBUSY != libc::EALREADY && error == libc::EALREADY {
        return Some(b"ealready\0");
    }
    if error == libc::EBUSY {
        return Some(b"ebusy\0");
    }
    if error == libc::ECONNREFUSED {
        return Some(b"econnrefused\0");
    }
    if libc::EWOULDBLOCK != libc::EDEADLK && error == libc::EDEADLK {
        return Some(b"edeadlk\0");
    }
    // Not a two-names collision but the same rule: the switch's `ERTS_ERRNO_CASE(ENOTSUP)` arm comes
    // *before* the value-guarded `EOPNOTSUPP` arm (`erl_errno_str.c:1078-1086`), so on a platform
    // where the two names share a value (`95` here) ERTS answers `enotsup` while a C library free to
    // choose may say `EOPNOTSUPP` (this one does). The arm is what keeps the answer the oracle's.
    if error == libc::ENOTSUP {
        return Some(b"enotsup\0");
    }
    None
}

/// The oracle's name for `error` when a **static table** knows it: the collision switch first (the
/// C's `errno_name()` order), then the built-in
/// [`errno_name_fallback`](errno_table::errno_name_fallback) table.
///
/// Both are static data, so a hit costs no allocation and no lock and its pointer is valid for the
/// process's lifetime. `0` is not here: the oracle pre-fills `errno_map[0]` with `errno_0` before its
/// name loop, so it never reaches a lookup (`erl_errno_str.c:1131-1152`, `:1233-1236`).
fn builtin_errno_name(error: c_int) -> Option<&'static [u8]> {
    colliding_errno_name(error).or_else(|| errno_table::errno_name_fallback(error))
}

/// ASCII-lowercase the NUL-terminated `name` into a stack buffer (no NUL), returning the buffer and
/// the number of bytes written — the C's `tolower_copy_estring` (`erl_errno_str.c:106-124`) at the
/// point the oracle stores a `strerrorname_np` answer, which is upper case at the source.
///
/// # Safety
///
/// `name` must be a NUL-terminated C string readable for the duration of this call.
pub(super) unsafe fn lowercase_into(name: *const c_char) -> ([u8; ERTS_UNKNOWN_ERRNO_BUF_SZ], usize) {
    let mut lower = [0u8; ERTS_UNKNOWN_ERRNO_BUF_SZ];
    let mut n = 0;
    while n < lower.len() - 1 {
        // SAFETY: `name` is NUL-terminated (caller contract), so the read stays inside its bytes.
        let c = unsafe { *name.add(n) };
        if c == 0 {
            break;
        }
        // Bit-preserving for any byte; only ASCII `A`..`Z` are folded (`'A' <= c && c <= 'Z'`).
        let b = c as u8;
        lower[n] = if b.is_ascii_uppercase() {
            b + (b'a' - b'A')
        } else {
            b
        };
        n += 1;
    }
    (lower, n)
}

/// Render the C's unknown-errno answer — `ERTS_UNKNOWN_ERRNO_FORMAT` `"errno_%d"`
/// (`erl_errno_str.c:47`, `:203-208`) — into `text`, returning the number of bytes written (no NUL).
///
/// No heap object is allocated: the digits are rendered into the caller's stack array (at most
/// `errno_` + `-` + ten digits = 17 bytes of the 29 usable ones).
fn render_unknown(error: c_int, text: &mut [u8; ERTS_UNKNOWN_ERRNO_BUF_SZ]) -> usize {
    const PREFIX: &[u8] = b"errno_";
    text[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut n = PREFIX.len();
    if error < 0 {
        text[n] = b'-';
        n += 1;
    }
    // `unsigned_abs` keeps `i32::MIN` from overflowing; the sign is emitted separately, as `%d` does.
    let mut value = error.unsigned_abs();
    let mut digits = [0u8; 10];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
        if value == 0 {
            break;
        }
    }
    while count > 0 {
        count -= 1;
        text[n] = digits[count];
        n += 1;
    }
    n
}

/// Copy `bytes` into a **permanent**, never-moving, NUL-terminated allocation and answer a reference
/// to it — the crate's `permanent_alloc`.
///
/// The box is intentionally leaked: the answer's lifetime is the process's (`erl_errno_str.c:1234-1237`),
/// so at most one allocation per *distinct* errno happens, and a `&'static [u8]` keeps the store
/// [`Send`]/[`Sync`] without an `unsafe impl`.
fn permanent_answer(bytes: &[u8]) -> &'static [u8] {
    let mut owned = Vec::with_capacity(bytes.len() + 1);
    owned.extend_from_slice(bytes);
    owned.push(0);
    Box::leak(owned.into_boxed_slice())
}

/// The permanently stored answer for a value **no** built-in table names: the C library's name
/// lowercased, else the `errno_<N>` form.
///
/// Called only on a store miss, under the write lock, so it may allocate.
fn rendered_errno_name(error: c_int) -> &'static [u8] {
    if let Some(lookup) = strerror_name_np() {
        // SAFETY: `lookup` is the resolved `strerrorname_np`; its answer is NULL or a NUL-terminated
        // string owned by the C library.
        let name = unsafe { lookup(error) };
        if !name.is_null() {
            // SAFETY: just checked — non-null and NUL-terminated.
            let (lower, n) = unsafe { lowercase_into(name) };
            return permanent_answer(&lower[..n]);
        }
    }
    let mut text = [0u8; ERTS_UNKNOWN_ERRNO_BUF_SZ];
    let n = render_unknown(error, &mut text);
    permanent_answer(&text[..n])
}

/// The NUL-terminated answer `erl_errno_id` hands out, permanently stored.
///
/// The C's route, in the C's order: `errno_map[0]`'s `errno_0` and the collision switch first, then a
/// name — the built-in table, which the C compiles in *instead of* the library probe when the library
/// has none, and which agrees with the library everywhere else (asserted by `errno_table`'s
/// cross-check test) — and finally the `errno_<N>` form for a value nobody names.
pub(super) fn errno_answer(error: c_int) -> &'static [u8] {
    if error == 0 {
        // `errno_map[0]` is pre-filled with `errno_0`, so `errno_name(0)` is never consulted
        // (`erl_errno_str.c:1146-1152`).
        return b"errno_0\0";
    }
    if let Some(name) = builtin_errno_name(error) {
        return name;
    }
    // The lock-free routes are exhausted: consult the permanent store (a `read` first, so a repeated
    // request for an already-rendered errno only ever takes the read side).
    if let Some(&answer) = ERRNO_ANSWERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&error)
    {
        return answer;
    }
    // `entry(..).or_insert_with(..)` under the write lock, so two threads racing on the same value
    // still leave exactly one leaked answer and both get *that* pointer.
    *ERRNO_ANSWERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(error)
        .or_insert_with(|| rendered_errno_name(error))
}

/// `char *erl_errno_id(int error);` — the errno's **lowercase POSIX name** (`"eacces"`, `"enoent"`,
/// `"eio"`, `"einval"`) and the form `errno_<N>` for any errno neither the built-in table nor the C
/// library can name — including `0`, which the C maps to `errno_0` because `errno_map[0]` is written
/// with `write_unknown_errno()` before its name loop starts (`erl_errno_str.c:1131-1152`,
/// `:1233-1236`).
///
/// The lookup order is the C's (`errno_name()`, `:979-1119`): the collision table
/// ([`colliding_errno_name`]) first, then a name — the built-in
/// [`errno_name_fallback`](errno_table::errno_name_fallback) table, unconditional in the C for
/// exactly the platforms whose C library has no `strerrorname_np` and in agreement with this
/// library's `strerrorname_np` on every value it names — and finally the `errno_<N>` form. Every
/// answer is ASCII-lowercased, because the oracle lowercases the C library's upper-case name on the
/// way into its own storage (`tolower_copy_estring`).
///
/// # Safety
///
/// The returned pointer is NUL-terminated and stays **valid and unchanged for the rest of the
/// process's lifetime** — the C's contract: *"Returned strings must not move or be deallocated during
/// the runtime systems lifetime"* (`erl_errno_str.c:1234-1237`). A built-in table's name points at
/// static read-only storage; anything rendered is boxed once and leaked. The caller must not free or
/// mutate it (a driver may keep several answers from different calls and read them all later).
#[no_mangle]
pub unsafe extern "C" fn erl_errno_id(error: c_int) -> *mut c_char {
    errno_answer(error).as_ptr() as *mut c_char
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::CStr;

    /// Read an `erl_errno_id` answer as a Rust string. The pointer is permanent (valid for the
    /// process's lifetime), so no test has to observe the "next call" discipline.
    fn errno_id(error: c_int) -> String {
        // SAFETY: `erl_errno_id` accepts any `int` and answers a NUL-terminated string that stays
        // valid, unchanged, for the rest of the process's lifetime.
        let p = unsafe { erl_errno_id(error) };
        assert!(
            !p.is_null(),
            "erl_errno_id({error}) must never answer NULL"
        );
        // SAFETY: non-null and NUL-terminated (the documented contract).
        let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
        // The oracle's answers are pure ASCII: the lowercased POSIX name or `errno_<N>`.
        assert!(
            bytes.is_ascii() && !bytes.is_empty(),
            "erl_errno_id({error}) must answer non-empty ASCII, got {bytes:?}"
        );
        String::from_utf8(bytes.to_vec()).expect("checked ASCII above")
    }

    #[test]
    fn errno_id_answers_the_lowercase_posix_name() {
        // The oracle's names (`erl_errno_str.c`'s `errno_name()` + `tolower_copy_estring`): the C
        // library's upper-case symbol, ASCII-lowercased.
        assert_eq!(errno_id(libc::EACCES), "eacces");
        assert_eq!(errno_id(libc::ENOENT), "enoent");
        assert_eq!(errno_id(libc::EINVAL), "einval");
        // The collision table resolves a value with more than one name to the oracle's spelling:
        // `EAGAIN` and `EWOULDBLOCK` are the same integer here, and the C has always answered
        // `eagain` for it.
        assert_eq!(
            libc::EAGAIN,
            libc::EWOULDBLOCK,
            "the premise of the collision case"
        );
        assert_eq!(errno_id(libc::EAGAIN), "eagain");
        assert_eq!(errno_id(libc::EWOULDBLOCK), "eagain");
        assert_eq!(errno_id(libc::EBUSY), "ebusy");
        assert_eq!(errno_id(libc::ECONNREFUSED), "econnrefused");
        assert_eq!(errno_id(libc::EDEADLK), "edeadlk");
        assert_eq!(errno_id(libc::EALREADY), "ealready");
        // Nothing resembling `strerror` prose may survive: no spaces, no upper case.
        for error in [libc::EACCES, libc::ENOENT, libc::EINVAL, libc::EAGAIN] {
            let answer = errno_id(error);
            assert!(
                answer
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "erl_errno_id({error}) = {answer:?} must be a bare lowercase name"
            );
        }
    }

    #[test]
    fn errno_id_answers_errno_n_for_anything_unnamed() {
        // `0` is the C's `errno_map[0]`: written with `write_unknown_errno()` before the name loop,
        // so it never reaches the lookup (`erl_errno_str.c:1146-1152`).
        assert_eq!(errno_id(0), "errno_0");
        // An errno this C library cannot name — the oracle's measured answer for `9999`.
        assert_eq!(errno_id(9999), "errno_9999");
        // Negative values take the same route (OTP's own `driver_SUITE_data/erl_errno_id_drv.c`
        // pins `-4711` and `-1`).
        assert_eq!(errno_id(-4711), "errno_-4711");
        assert_eq!(errno_id(-1), "errno_-1");
        // The extreme still fits the buffer and prints in full.
        assert_eq!(errno_id(c_int::MIN), format!("errno_{}", c_int::MIN));
        assert_eq!(errno_id(c_int::MAX), format!("errno_{}", c_int::MAX));
    }

    #[test]
    fn errno_id_answers_are_permanent_and_stable_across_calls() {
        // **Strengthened contract.** This replaces a test that only pinned "the answer stays readable
        // until the next call" — a weaker promise than the C makes. `erl_errno_id`'s own comment is
        // explicit (`erl_errno_str.c:1234-1237`): *"Returned strings must not move or be deallocated
        // during the runtime systems lifetime"*, because a driver may cache several answers from
        // different calls and read them all later (the C backs that with `errno_map` plus a hash
        // table whose free function is a no-op). So each of these pointers must survive **every**
        // later call, not just the next one.
        // SAFETY: any `int` is accepted; every answer is permanent, so all of these stay live.
        let (eacces, enoent, unknown) = unsafe {
            (
                erl_errno_id(libc::EACCES),
                erl_errno_id(libc::ENOENT),
                erl_errno_id(9999),
            )
        };
        assert_ne!(eacces, enoent, "two names never share one answer buffer");
        // Three later calls happened between the first two and these reads.
        // SAFETY: all three answers are permanent (the documented contract).
        unsafe {
            assert_eq!(CStr::from_ptr(eacces).to_bytes(), b"eacces");
            assert_eq!(CStr::from_ptr(enoent).to_bytes(), b"enoent");
            assert_eq!(CStr::from_ptr(unknown).to_bytes(), b"errno_9999");
        }

        // Repeated calls for the same errno answer the same pointer — for a static table name, for
        // the pre-filled `errno_0` and for a rendered one (which is stored, not re-rendered).
        // SAFETY: as above.
        for error in [libc::EACCES, libc::ENOENT, libc::ENOTSUP, libc::EAGAIN, 0, 9999] {
            // SAFETY: as above.
            let (first, second) = unsafe { (erl_errno_id(error), erl_errno_id(error)) };
            assert_eq!(first, second, "erl_errno_id({error}) moved its answer");
        }

        // A *different* unnamed errno gets its own storage, and the earlier one is still intact.
        // SAFETY: as above.
        let other = unsafe { erl_errno_id(9998) };
        assert_ne!(other, unknown, "each distinct errno has its own permanent answer");
        // SAFETY: as above.
        unsafe {
            assert_eq!(CStr::from_ptr(other).to_bytes(), b"errno_9998");
            assert_eq!(CStr::from_ptr(unknown).to_bytes(), b"errno_9999");
            assert_eq!(CStr::from_ptr(eacces).to_bytes(), b"eacces");
        }
    }

    #[test]
    fn the_builtin_errno_names_match_the_c_name_table() {
        // The ported C tables — `errno_name()`'s collision switch plus `errno_name_fallback`
        // (`erl_errno_str.c:395-977`, `:979-1119`) — are what answers on a C library without
        // `strerrorname_np`, and what pins the collision spellings everywhere else. Every name below
        // is the C's own source spelling, ASCII-lowercased (`tolower_copy_estring`).
        for (error, want) in [
            // The collision switch (consulted before the library).
            (libc::EACCES, "eacces"),
            (libc::ENOENT, "enoent"),
            (libc::EINVAL, "einval"),
            (libc::EIO, "eio"),
            (libc::EAGAIN, "eagain"),
            (libc::EBUSY, "ebusy"),
            (libc::ECONNREFUSED, "econnrefused"),
            (libc::EDEADLK, "edeadlk"),
            (libc::EALREADY, "ealready"),
            // `ERTS_ERRNO_CASE(ENOTSUP)` precedes the guarded `EOPNOTSUPP` arm, so the two names
            // sharing value `95` on this platform answer `enotsup` (`erl_errno_str.c:1078-1086`).
            (libc::ENOTSUP, "enotsup"),
            // The POSIX group, two of the less common ones included.
            (libc::ENOTRECOVERABLE, "enotrecoverable"),
            (libc::EOWNERDEAD, "eownerdead"),
            (libc::EDQUOT, "edquot"),
            (libc::ENAMETOOLONG, "enametoolong"),
            // The Linux group.
            (libc::EUCLEAN, "euclean"),
            (libc::EKEYREJECTED, "ekeyrejected"),
            (libc::EHWPOISON, "ehwpoison"),
        ] {
            let got = builtin_errno_name(error)
                .unwrap_or_else(|| panic!("the built-in table must name errno {error}"));
            // The stored form is NUL-terminated (it is handed to C as it stands), so the trailing NUL
            // is part of the answer: exactly one, at the end.
            let mut terminated = want.as_bytes().to_vec();
            terminated.push(0);
            assert_eq!(
                got,
                &terminated[..],
                "the built-in name for errno {error} (with the C's trailing NUL)"
            );
        }

        // Values no table names: `0` (the C's forced `errno_0`), `41`/`58` (unused in the Linux
        // errno numbering) and anything out of range, so they take the `errno_<N>` form.
        for error in [0, 41, 58, 9999, -1, c_int::MIN, c_int::MAX] {
            assert!(
                builtin_errno_name(error).is_none(),
                "errno {error} is not named by any built-in table"
            );
        }
    }
}