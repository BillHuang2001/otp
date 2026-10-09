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

//! `pub(crate)` error constructors — the crate adds no error type of its own.
//!
//! Every fallible path in `beam_io` returns the workspace error type
//! ([`BeamError`](beam_common::BeamError)); this module only centralises the
//! construction of the failures the I/O layer can raise, exactly as
//! `beam_process::error` and `beam_message::error` do for their crates. The mapping is
//! fixed by the crate `CONTEXT.md`:
//!
//! * an I/O or syscall failure (`read`/`write`/`pipe`/registration) ⇒ [`io`]
//!   (`BeamError::Io`),
//! * an argument fault (a bad fd, a bad interest mask, an unknown driver name) ⇒
//!   [`badarg`] (`BeamError::BadArg`),
//! * id or poll-registration exhaustion ⇒ [`system_limit`] (`BeamError::SystemLimit`),
//! * a failed OS-child spawn ⇒ [`spawn_failure`]: the errno-derived BEAM atom the oracle
//!   raises (`error:enoent`, `error:eacces`, …), falling back to [`io`] for an errno this
//!   crate does not recognise.
//!
//! `mio` and `std` report their failures as [`std::io::Error`]; callers hand those to
//! [`io`] so the crate never grows a second error enum.

use beam_common::{BeamError, ExitReason, Raise};

/// The error for an I/O or syscall failure (`BeamError::Io`).
pub(crate) fn io(err: std::io::Error) -> BeamError {
    BeamError::Io(err)
}

/// The error for an argument fault: a bad fd, a bad interest mask or an unknown driver
/// name (`badarg`).
pub(crate) fn badarg() -> BeamError {
    BeamError::BadArg
}

/// The error raised when the port/registration number space is exhausted (`system_limit`).
pub(crate) fn system_limit() -> BeamError {
    BeamError::SystemLimit
}

/// The error for a capability this crate does not model yet: the `notsup` raise.
///
/// Used where the oracle *would* succeed but Milestone-1 has no mechanism for it — e.g.
/// creating a port from a **statically linked** driver entry, which needs a
/// [`Driver`](crate::driver::Driver) adapter over a raw `ErlDrvEntry` this crate does not
/// have. A raise (rather than a silent `badarg`) keeps the gap distinguishable from a wrong
/// argument, and never fabricates a port.
pub(crate) fn notsup() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("notsup")))
}

/// The typed "not implemented" diagnostic: a BEAM `error` whose reason is the atom
/// `unsupported: <what>`.
///
/// This is the crate's standard diagnostic for a genuine **capability gap** in the I/O
/// layer — distinct from [`notsup`], which is the oracle's own atom for a limitation it
/// really reports. It is deliberately a [`BeamError::Raise`] rather than a silent default,
/// so a caller always gets a typed error instead of a *wrong* result (the workspace §3
/// error strategy); the reason string is the single place the `unsupported:` prefix is
/// spelled, matching `beam_process::error::unsupported` and `beam_rt::error::unsupported`.
pub(crate) fn unsupported(what: &str) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(format!("unsupported: {what}"))))
}

/// The error for a driver entry whose `init` callback refused registration
/// (`driver_init_failed`).
///
/// The oracle's `erts_add_driver_entry` drops the entry again and returns the callback's
/// non-zero result; the load path renders that as `{error,driver_init_failed}`
/// (`erl_bif_ddll.c:1519-1525`). Carrying the same atom keeps the failure observable.
pub(crate) fn driver_init_failed() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("driver_init_failed")))
}

/// The BEAM atom name the system Erlang reports for a POSIX `errno`, or `None` for a code
/// this crate does not recognise.
///
/// The oracle asks the libc for the errno *name* and lowercases it (`erl_errno_id` →
/// `tolower_copy_estring(errno_name(eno))`, `erts/emulator/beam/erl_errno_str.c`), so the
/// atom is exactly the lowercase of the C macro: `ENOENT` → `enoent`, `EACCES` → `eacces`,
/// `ENOTDIR` → `enotdir`, `E2BIG` → `e2big`, … The table below covers the codes a `fork`/
/// `exec`/`pipe`/`access`/`getcwd` can actually fail with. Anything else is left
/// unrecognised, so the caller keeps the raw [`io`] failure instead of inventing an atom
/// (the oracle's own fallback name is `errno_<N>`, which this crate deliberately does not
/// synthesize).
pub(crate) fn errno_atom(errno: i32) -> Option<&'static str> {
    let name = match errno {
        libc::E2BIG => "e2big",
        libc::EACCES => "eacces",
        libc::EAGAIN => "eagain",
        libc::EBADF => "ebadf",
        libc::EBUSY => "ebusy",
        libc::ECHILD => "echild",
        libc::EDQUOT => "edquot",
        libc::EEXIST => "eexist",
        libc::EFAULT => "efault",
        libc::EFBIG => "efbig",
        libc::EINTR => "eintr",
        libc::EINVAL => "einval",
        libc::EIO => "eio",
        libc::EISDIR => "eisdir",
        libc::ELOOP => "eloop",
        libc::EMFILE => "emfile",
        libc::EMLINK => "emlink",
        libc::ENAMETOOLONG => "enametoolong",
        libc::ENFILE => "enfile",
        libc::ENOENT => "enoent",
        libc::ENOEXEC => "enoexec",
        libc::ENOMEM => "enomem",
        libc::ENOSPC => "enospc",
        libc::ENOSYS => "enosys",
        libc::ENOTDIR => "enotdir",
        libc::ENXIO => "enxio",
        libc::EPERM => "eperm",
        libc::EPIPE => "epipe",
        libc::EROFS => "erofs",
        libc::ESRCH => "esrch",
        libc::ETIMEDOUT => "etimedout",
        libc::ETXTBSY => "etxtbsy",
        libc::EXDEV => "exdev",
        _ => return None,
    };
    Some(name)
}

/// The error for a failed OS-child spawn: the errno-derived atom raise the oracle produces.
///
/// `erts_internal_open_port_2` answers `erl_errno_id(err_num)` (`erl_bif_port.c:84`) — a BEAM
/// error whose **reason is the errno atom**, not a generic failure — so this builds
/// `BeamError::Raise(Raise::error(ExitReason::atom("enoent")))`. An errno [`errno_atom`] does
/// not recognise (or an error that carries no OS code at all) keeps the raw [`io`] failure,
/// so a spawn failure is never silently rewritten into a wrong atom.
pub(crate) fn spawn_failure(err: std::io::Error) -> BeamError {
    match err.raw_os_error().and_then(errno_atom) {
        Some(name) => BeamError::Raise(Raise::error(ExitReason::atom(name))),
        None => io(err),
    }
}

#[cfg(test)]
mod tests {
    use super::{errno_atom, spawn_failure};
    use beam_common::{BeamError, ExceptionClass, ExitReason};

    #[test]
    fn the_errno_atom_is_the_lowercased_posix_name() {
        assert_eq!(errno_atom(libc::ENOENT), Some("enoent"));
        assert_eq!(errno_atom(libc::EACCES), Some("eacces"));
        assert_eq!(errno_atom(libc::ENOTDIR), Some("enotdir"));
        assert_eq!(errno_atom(libc::EISDIR), Some("eisdir"));
        assert_eq!(errno_atom(libc::EMFILE), Some("emfile"));
        assert_eq!(errno_atom(libc::ENOMEM), Some("enomem"));
        // The lowercasing rules (`E2BIG` → `e2big`, `ENAMETOOLONG` → `enametoolong`).
        assert_eq!(errno_atom(libc::E2BIG), Some("e2big"));
        assert_eq!(errno_atom(libc::ENAMETOOLONG), Some("enametoolong"));
    }

    #[test]
    fn an_errno_the_reference_names_only_generically_is_not_mapped() {
        // 4711 has no POSIX name, so it stays unrecognised rather than becoming an atom
        // (the oracle's `errno_4711` is deliberately not synthesized).
        assert_eq!(errno_atom(4711), None);
    }

    #[test]
    fn a_failed_spawn_is_the_errno_atom_raise() {
        let error = spawn_failure(std::io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(error.as_atom(), Some("enoent"));
        let raise = error.into_raise().expect("a raise, not a generic failure");
        assert_eq!(raise.class, ExceptionClass::Error);
        assert_eq!(raise.reason, ExitReason::atom("enoent"));
    }

    #[test]
    fn an_unrecognised_errno_keeps_the_io_failure() {
        assert!(matches!(
            spawn_failure(std::io::Error::from_raw_os_error(4711)),
            BeamError::Io(_)
        ));
        // An error that carries no OS code at all cannot name an atom either.
        assert!(matches!(
            spawn_failure(std::io::Error::new(
                std::io::ErrorKind::Other,
                "no errno",
            )),
            BeamError::Io(_)
        ));
    }
}
