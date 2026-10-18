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

//! The **built-in errno name table** — a port of the C's errno→name mapping that does not need the C
//! library (`erts/emulator/beam/erl_errno_str.c`, the [`errno_name_fallback`] half of
//! [`errno_name()`](super::errno_answer)).
//!
//! # Why it exists
//!
//! The C reaches an errno's name in one of two ways, selected at build time
//! (`erl_errno_str.c:43-45`, `:1109-1115`): the C library's `strerrorname_np` when the platform has
//! one (`ERTS_HAVE_ERRNO_NAME_LOOKUP_FUNC`), and otherwise the built-in `errno_name_fallback` switch
//! (`:395-977`). `errno_name()`'s own switch (`:979-1119`) — the collision spellings — precedes both.
//! So the built-in table is not a nicety: **on a C library without `strerrorname_np` (musl, glibc
//! older than 2.32) it is the whole answer**, and `errno_name()` never returns `NULL` for a value it
//! knows. Answering `errno_<N>` there would name an errno the oracle names (`enoent` for `ENOENT`).
//!
//! [`super`] therefore consults this table *before* probing the C library, and falls back to
//! [`errno_<N>`](super::erl_errno_id) only for a value no table names. Both routes agree on every
//! value the table names, which the cross-check test at the bottom of this file asserts against this
//! platform's own `strerrorname_np`, so neither side can drift unnoticed.
//!
//! # What is here
//!
//! The union of the C's two sources, in the C's own order, one entry per errno **value**:
//!
//! * the `errno_name()` switch's arms that the collision table
//!   ([`super::colliding_errno_name`]) does not already answer — those are the spellings that only
//!   matter when the library is missing, because on a working library they are the same name anyway;
//! * `errno_name_fallback`'s POSIX.1-2017 group, then its per-OS groups, guarded exactly as the C
//!   guards them — a name the C only compiles `#ifdef`ed in is a name that can only be listed when
//!   this platform defines it, and the `libc` constants supply that condition;
//! * the six collision spellings (`EAGAIN`/`EALREADY`/`EBUSY`/`ECONNREFUSED`/`EDEADLK`/`ENOTSUP`)
//!   are deliberately **not** here — [`super::colliding_errno_name`] answers them before this table
//!   is consulted, and `EDEADLOCK`'s arm is dropped because it is the same value as `EDEADLK` on
//!   every platform that defines both.
//!
//! # Storage and cost
//!
//! Every name is a `&'static [u8]` literal that already ends in a NUL, so an answer points straight
//! into read-only static data: nothing is allocated, no lock is taken, and the pointer stays valid for
//! the process's lifetime — the C's *"Returned strings must not move or be deallocated during the
//! runtime systems lifetime"* (`erl_errno_str.c:1234-1237`). The lookup is a linear scan over 126
//! entries on the (cold) errno path; a value's entry is unique, so scan order can never change an
//! answer.

use libc::c_int;
/// `EFTYPE` as glibc declares it (`134`). It is the one name this table needs that the `libc` crate
/// does not itself declare, so the number is spelled here — and pinned against this C library's own
/// `strerrorname_np` by [`tests::the_builtin_table_agrees_with_the_c_librarys_errno_names`], which is
/// what keeps a mistyped number from silently naming the wrong errno.
#[cfg(target_os = "linux")]
const EFTYPE: c_int = 134;

/// The errno values this platform names, with the **lowercase, NUL-terminated** bytes the oracle
/// answers for each (the C lowercases its upper-case source spelling with `tolower_copy_estring`,
/// `erl_errno_str.c:106-124`).
static ERRNO_NAMES: &[(c_int, &[u8])] = &[
    // the `errno_name()` switch, whose arms precede the library probe.
    (libc::EEXIST, b"eexist\0"),
    (libc::EIDRM, b"eidrm\0"),
    (libc::EINPROGRESS, b"einprogress\0"),
    (libc::ELOOP, b"eloop\0"),
    (libc::ENAMETOOLONG, b"enametoolong\0"),
    (libc::ENOBUFS, b"enobufs\0"),
    (libc::ENOSTR, b"enostr\0"),
    (libc::ENODATA, b"enodata\0"),
    (libc::ENOENT, b"enoent\0"),
    (libc::ENOLCK, b"enolck\0"),
    (libc::ENOSR, b"enosr\0"),
    (libc::ENOTEMPTY, b"enotempty\0"),
    (libc::ENOTTY, b"enotty\0"),
    (libc::ETIME, b"etime\0"),
    (libc::ETIMEDOUT, b"etimedout\0"),
    // POSIX.1-2017 names (`errno_name_fallback`).
    (libc::E2BIG, b"e2big\0"),
    (libc::EACCES, b"eacces\0"),
    (libc::EADDRINUSE, b"eaddrinuse\0"),
    (libc::EADDRNOTAVAIL, b"eaddrnotavail\0"),
    (libc::EAFNOSUPPORT, b"eafnosupport\0"),
    (libc::EBADF, b"ebadf\0"),
    (libc::EBADMSG, b"ebadmsg\0"),
    (libc::ECANCELED, b"ecanceled\0"),
    (libc::ECHILD, b"echild\0"),
    (libc::ECONNABORTED, b"econnaborted\0"),
    (libc::ECONNRESET, b"econnreset\0"),
    (libc::EDESTADDRREQ, b"edestaddrreq\0"),
    (libc::EDOM, b"edom\0"),
    (libc::EDQUOT, b"edquot\0"),
    (libc::EFAULT, b"efault\0"),
    (libc::EFBIG, b"efbig\0"),
    (libc::EHOSTUNREACH, b"ehostunreach\0"),
    (libc::EILSEQ, b"eilseq\0"),
    (libc::EINTR, b"eintr\0"),
    (libc::EINVAL, b"einval\0"),
    (libc::EIO, b"eio\0"),
    (libc::EISCONN, b"eisconn\0"),
    (libc::EISDIR, b"eisdir\0"),
    (libc::EMFILE, b"emfile\0"),
    (libc::EMLINK, b"emlink\0"),
    (libc::EMSGSIZE, b"emsgsize\0"),
    (libc::EMULTIHOP, b"emultihop\0"),
    (libc::ENETDOWN, b"enetdown\0"),
    (libc::ENETRESET, b"enetreset\0"),
    (libc::ENETUNREACH, b"enetunreach\0"),
    (libc::ENFILE, b"enfile\0"),
    (libc::ENODEV, b"enodev\0"),
    (libc::ENOEXEC, b"enoexec\0"),
    (libc::ENOLINK, b"enolink\0"),
    (libc::ENOMEM, b"enomem\0"),
    (libc::ENOMSG, b"enomsg\0"),
    (libc::ENOPROTOOPT, b"enoprotoopt\0"),
    (libc::ENOSPC, b"enospc\0"),
    (libc::ENOSYS, b"enosys\0"),
    (libc::ENOTCONN, b"enotconn\0"),
    (libc::ENOTDIR, b"enotdir\0"),
    (libc::ENOTRECOVERABLE, b"enotrecoverable\0"),
    (libc::ENOTSOCK, b"enotsock\0"),
    (libc::ENXIO, b"enxio\0"),
    (libc::EOVERFLOW, b"eoverflow\0"),
    (libc::EOWNERDEAD, b"eownerdead\0"),
    (libc::EPERM, b"eperm\0"),
    (libc::EPIPE, b"epipe\0"),
    (libc::EPROTO, b"eproto\0"),
    (libc::EPROTONOSUPPORT, b"eprotonosupport\0"),
    (libc::EPROTOTYPE, b"eprototype\0"),
    (libc::ERANGE, b"erange\0"),
    (libc::EROFS, b"erofs\0"),
    (libc::ESPIPE, b"espipe\0"),
    (libc::ESRCH, b"esrch\0"),
    (libc::ESTALE, b"estale\0"),
    (libc::ETXTBSY, b"etxtbsy\0"),
    (libc::EXDEV, b"exdev\0"),
    // FreeBSD names (and not above).
    (libc::ENOTBLK, b"enotblk\0"),
    (libc::ESOCKTNOSUPPORT, b"esocktnosupport\0"),
    (libc::EPFNOSUPPORT, b"epfnosupport\0"),
    (libc::ESHUTDOWN, b"eshutdown\0"),
    (libc::ETOOMANYREFS, b"etoomanyrefs\0"),
    (libc::EHOSTDOWN, b"ehostdown\0"),
    (libc::EUSERS, b"eusers\0"),
    (libc::EREMOTE, b"eremote\0"),
    #[cfg(target_os = "linux")]
    (EFTYPE, b"eftype\0"),
    // OpenBSD names (and not above).
    (libc::ENOMEDIUM, b"enomedium\0"),
    (libc::EMEDIUMTYPE, b"emediumtype\0"),
    // illumos names (and not above).
    (libc::ECHRNG, b"echrng\0"),
    (libc::EL2NSYNC, b"el2nsync\0"),
    (libc::EL3HLT, b"el3hlt\0"),
    (libc::EL3RST, b"el3rst\0"),
    (libc::ELNRNG, b"elnrng\0"),
    (libc::EUNATCH, b"eunatch\0"),
    (libc::ENOCSI, b"enocsi\0"),
    (libc::EL2HLT, b"el2hlt\0"),
    (libc::EBADE, b"ebade\0"),
    (libc::EBADR, b"ebadr\0"),
    (libc::EXFULL, b"exfull\0"),
    (libc::ENOANO, b"enoano\0"),
    (libc::EBADRQC, b"ebadrqc\0"),
    (libc::EBADSLT, b"ebadslt\0"),
    (libc::EBFONT, b"ebfont\0"),
    (libc::ENONET, b"enonet\0"),
    (libc::ENOPKG, b"enopkg\0"),
    (libc::EADV, b"eadv\0"),
    (libc::ESRMNT, b"esrmnt\0"),
    (libc::ECOMM, b"ecomm\0"),
    (libc::ENOTUNIQ, b"enotuniq\0"),
    (libc::EBADFD, b"ebadfd\0"),
    (libc::EREMCHG, b"eremchg\0"),
    (libc::ELIBACC, b"elibacc\0"),
    (libc::ELIBBAD, b"elibbad\0"),
    (libc::ELIBSCN, b"elibscn\0"),
    (libc::ELIBMAX, b"elibmax\0"),
    (libc::ELIBEXEC, b"elibexec\0"),
    (libc::ERESTART, b"erestart\0"),
    (libc::ESTRPIPE, b"estrpipe\0"),
    // Linux names (and not above).
    (libc::EDOTDOT, b"edotdot\0"),
    (libc::EUCLEAN, b"euclean\0"),
    (libc::ENOTNAM, b"enotnam\0"),
    (libc::ENAVAIL, b"enavail\0"),
    (libc::EISNAM, b"eisnam\0"),
    (libc::EREMOTEIO, b"eremoteio\0"),
    (libc::ENOKEY, b"enokey\0"),
    (libc::EKEYEXPIRED, b"ekeyexpired\0"),
    (libc::EKEYREVOKED, b"ekeyrevoked\0"),
    (libc::EKEYREJECTED, b"ekeyrejected\0"),
    (libc::ERFKILL, b"erfkill\0"),
    (libc::EHWPOISON, b"ehwpoison\0"),
];

/// `errno_name_fallback(eno)` — the built-in name of `eno`, or `None` when this table does not know
/// it (the C's `return NULL`, `erl_errno_str.c:971-973`, after which the oracle answers `errno_<N>`).
///
/// The answer is NUL-terminated and points at static storage, so it is valid for the process's
/// lifetime.
pub(super) fn errno_name_fallback(error: c_int) -> Option<&'static [u8]> {
    ERRNO_NAMES
        .iter()
        .find(|(value, _)| *value == error)
        .map(|(_, name)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The table cannot drift from the C library.** Every value this table names is also a value
    /// this platform's `strerrorname_np` names, and the two spellings must be identical once the
    /// library's upper case is folded down — so a transcription slip (a wrong `libc` constant, a
    /// mistyped name, a stale `EFTYPE` number) fails here instead of answering a wrong errno name in
    /// production. The collision spellings are `super::super::errno::colliding_errno_name`'s job and
    /// are not in this table, so no exception is needed: on this platform the library and the table
    /// agree on **every** entry.
    #[test]
    fn the_builtin_table_agrees_with_the_c_librarys_errno_names() {
        let Some(lookup) = super::super::errno::strerror_name_np() else {
            eprintln!(
                "SKIP: this C library exports no strerrorname_np (musl / glibc < 2.32), so the \
                 built-in table has no local cross-check — it is the answer on such a system"
            );
            return;
        };
        // The set is large and complete for this platform's errno range, so a mangled table cannot
        // hide behind a small sample.
        assert_eq!(ERRNO_NAMES.len(), 126, "the built-in table's size");
        for &(value, name) in ERRNO_NAMES {
            // SAFETY: `lookup` is the resolved `strerrorname_np`; it accepts any `int` and answers
            // NULL or a NUL-terminated string owned by the C library.
            let library_name = unsafe { lookup(value) };
            assert!(
                !library_name.is_null(),
                "the built-in table names errno {value} but strerrorname_np does not"
            );
            // SAFETY: just checked — non-null and NUL-terminated.
            let (lower, n) = unsafe { super::super::errno::lowercase_into(library_name) };
            // `name` is the stored form (NUL-terminated); `lower[..n]` is the library's name with the
            // NUL excluded, so compare the text only.
            assert_eq!(
                &name[..name.len() - 1],
                &lower[..n],
                "the built-in name for errno {value} drifted from strerrorname_np"
            );
        }
        // And the values the table does not know are values this C library does not name either —
        // `41`/`58` are unused in the Linux errno numbering, and anything out of range or negative is
        // outside its table — so both sides answer `errno_<N>`.
        for value in [41, 58, -1, 9999] {
            assert!(
                errno_name_fallback(value).is_none(),
                "value {value}, not named here"
            );
            // SAFETY: as above — any `int` is accepted.
            assert!(
                unsafe { lookup(value) }.is_null(),
                "strerrorname_np names errno {value}, so the table should too"
            );
        }
        // `0` is deliberately absent from the table and is the one value the library *does* name
        // where the oracle does not consult it: `strerrorname_np(0)` says `"0"`, while ERTS answers
        // `errno_0` because `errno_map[0]` is pre-filled before the name loop
        // (`erl_errno_str.c:1131-1152`, `:1233-1236`). [`super::super::errno::errno_answer`] answers
        // `errno_0` first, never reaching either lookup — so the difference cannot leak.
        assert!(errno_name_fallback(0).is_none(), "0 is the oracle's forced answer");
        assert_eq!(super::super::errno::errno_answer(0), b"errno_0\0");
    }

    /// A spot check that the table holds *names*, not `errno_<N>` fallbacks, and that each entry is
    /// NUL-terminated exactly once (the shape [`super::super::erl_errno_id`] hands to C). The C's
    /// names are ASCII-lowercase and some carry a digit (`e2big`), never upper case or punctuation.
    #[test]
    fn every_entry_is_a_nul_terminated_lowercase_name() {
        for &(value, name) in ERRNO_NAMES {
            assert_eq!(name.last(), Some(&0), "errno {value} is not NUL-terminated");
            let text = &name[..name.len() - 1];
            assert!(!text.is_empty(), "errno {value} has an empty name");
            assert!(
                text.iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "errno {value} is not a bare lowercase name: {text:?}"
            );
            assert!(
                !name[..name.len() - 1].contains(&0),
                "errno {value} has an interior NUL"
            );
        }
        // No value appears twice (a duplicate would make the scan order decide the answer).
        for (i, (value, _)) in ERRNO_NAMES.iter().enumerate() {
            for (other, _) in &ERRNO_NAMES[i + 1..] {
                assert_ne!(value, other, "errno {value} appears twice");
            }
        }
    }
}
