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

/// The crate's **single** errno→name table: one row per code the reference can name.
///
/// Every view of an errno in this crate is derived from this table — [`errno_atom`] (the
/// `Option` form the `{error, Atom}` renders need) and [`errno_name`] (the raw string form the
/// inet driver's `[0] ++ Name` replies carry). Nothing else may spell a name.
///
/// The names are the oracle's own renderings (`erl_errno_id`, i.e. the lowercase of the libc's
/// `strerrorname_np` name — `erts/emulator/beam/erl_errno_str.c`), measured against
/// OTP 29.1.1/erts-17.1. Three properties are worth knowing:
///
/// * **The table is the union of what `fork`/`exec`/`pipe`/`access`/`getcwd` can fail with and
///   what a socket can fail with**, so both the spawn floor and the inet driver render through
///   it.
/// * **Codes the oracle answers with the generic `errno_<N>` form are absent** — `0`, `-1` and
///   any code with no POSIX name. [`errno_atom`] therefore answers `None` and the caller keeps
///   the raw [`BeamError::Io`] rather than inventing an atom; [`errno_name`] spells the
///   reference's `ERTS_UNKNOWN_ERRNO_FORMAT` (`"errno_%d"`) itself.
/// * **A value with two POSIX spellings has exactly one row**, carrying the name the reference
///   picks. The reference keeps a hand-written collision list *in front of* the libc lookup for
///   exactly this reason (`errno_name()` in `erl_errno_str.c`): on Linux `ENOTSUP` and
///   `EOPNOTSUPP` are the same value (`95`) and the list answers `ENOTSUP` ⇒ **`enotsup`**, not
///   `eopnotsupp` (measured; the emulator's own `erl_errno_id_drv.c` allows the same alternate
///   with `TEST_ERRNO_ALT1(EOPNOTSUPP, "enotsup")`). Likewise `EWOULDBLOCK` is `EAGAIN` on Linux,
///   so its single row is [`libc::EAGAIN`] ⇒ `eagain`.
const ERRNO_NAMES: &[(i32, &str)] = &[
    // The `fork`/`exec`/`pipe`/`access`/file set.
    (libc::E2BIG, "e2big"),
    (libc::EACCES, "eacces"),
    (libc::EAGAIN, "eagain"),
    (libc::EBADF, "ebadf"),
    (libc::EBUSY, "ebusy"),
    (libc::ECHILD, "echild"),
    (libc::EDQUOT, "edquot"),
    (libc::EEXIST, "eexist"),
    (libc::EFAULT, "efault"),
    (libc::EFBIG, "efbig"),
    (libc::EILSEQ, "eilseq"),
    (libc::EINTR, "eintr"),
    (libc::EINVAL, "einval"),
    (libc::EIO, "eio"),
    (libc::EISDIR, "eisdir"),
    (libc::ELOOP, "eloop"),
    (libc::EMFILE, "emfile"),
    (libc::EMLINK, "emlink"),
    (libc::ENAMETOOLONG, "enametoolong"),
    (libc::ENFILE, "enfile"),
    (libc::ENODEV, "enodev"),
    (libc::ENOENT, "enoent"),
    (libc::ENOEXEC, "enoexec"),
    (libc::ENOMEM, "enomem"),
    (libc::ENOSPC, "enospc"),
    (libc::ENOSYS, "enosys"),
    (libc::ENOTDIR, "enotdir"),
    (libc::ENOTEMPTY, "enotempty"),
    (libc::ENOTTY, "enotty"),
    (libc::ENXIO, "enxio"),
    (libc::EPERM, "eperm"),
    (libc::EPIPE, "epipe"),
    (libc::EROFS, "erofs"),
    (libc::ESPIPE, "espipe"),
    (libc::ESRCH, "esrch"),
    (libc::ETIMEDOUT, "etimedout"),
    (libc::ETXTBSY, "etxtbsy"),
    (libc::EXDEV, "exdev"),
    // The socket set `tcp_inet`/`udp_inet` can produce.
    (libc::EADDRINUSE, "eaddrinuse"),
    (libc::EADDRNOTAVAIL, "eaddrnotavail"),
    (libc::EAFNOSUPPORT, "eafnosupport"),
    (libc::EALREADY, "ealready"),
    (libc::ECONNABORTED, "econnaborted"),
    (libc::ECONNREFUSED, "econnrefused"),
    (libc::ECONNRESET, "econnreset"),
    (libc::EDESTADDRREQ, "edestaddrreq"),
    (libc::EHOSTDOWN, "ehostdown"),
    (libc::EHOSTUNREACH, "ehostunreach"),
    (libc::EINPROGRESS, "einprogress"),
    (libc::EISCONN, "eisconn"),
    (libc::EMSGSIZE, "emsgsize"),
    (libc::ENETDOWN, "enetdown"),
    (libc::ENETRESET, "enetreset"),
    (libc::ENETUNREACH, "enetunreach"),
    (libc::ENOBUFS, "enobufs"),
    (libc::ENOPROTOOPT, "enoprotoopt"),
    (libc::ENOTCONN, "enotconn"),
    (libc::ENOTSOCK, "enotsock"),
    // Linux: `ENOTSUP == EOPNOTSUPP == 95` and the reference's collision list answers
    // `ENOTSUP`, so the atom is `enotsup` (see the table docs above).
    (libc::EOPNOTSUPP, "enotsup"),
    (libc::EPROTONOSUPPORT, "eprotonosupport"),
    (libc::EPROTOTYPE, "eprototype"),
];

/// The BEAM atom name the system Erlang reports for a POSIX `errno`, or `None` for a code
/// this crate does not recognise.
///
/// The oracle asks the libc for the errno *name* and lowercases it (`erl_errno_id` →
/// `tolower_copy_estring(errno_name(eno))`, `erts/emulator/beam/erl_errno_str.c`), so the
/// atom is exactly the lowercase of the C macro: `ENOENT` → `enoent`, `EACCES` → `eacces`,
/// `ECONNREFUSED` → `econnrefused`, `ENAMETOOLONG` → `enametoolong`, … The rows are the crate's
/// single [`ERRNO_NAMES`] table, i.e. the codes a `fork`/`exec`/`pipe`/`access`/`getcwd` **and**
/// a socket can fail with.
///
/// A code outside the table is left unrecognised, so the caller keeps the raw [`io`] failure
/// instead of inventing an atom (the oracle's own fallback name is `errno_<N>`, which
/// [`errno_name`] spells for the callers that need the string).
pub(crate) fn errno_atom(errno: i32) -> Option<&'static str> {
    ERRNO_NAMES
        .iter()
        .find(|(code, _)| *code == errno)
        .map(|(_, name)| *name)
}

/// The raw name the oracle renders for an `errno`: the same table [`errno_atom`] reads, with
/// the reference's `ERTS_UNKNOWN_ERRNO_FORMAT` (`"errno_%d"`) fallback for a code it cannot
/// name.
///
/// This is the form the inet driver's `[0] ++ Name` replies carry (`ctl_error`'s payload,
/// `inet_drv.c:2300`) — `erl_errno_id`'s answer, which is `errno_0`/`errno_4711`/`errno_-1` for
/// the codes the emulator itself renders generically (measured; `erl_errno_id_drv.c`'s
/// `TEST_ERRNO_UNKNOWN` pins the same three).
pub(crate) fn errno_name(errno: i32) -> String {
    match errno_atom(errno) {
        Some(name) => name.to_string(),
        None => format!("errno_{errno}"),
    }
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
    use super::{errno_atom, errno_name, spawn_failure, ERRNO_NAMES};
    use beam_common::{BeamError, ExceptionClass, ExitReason};
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

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

    // -------------------------------------------------------------------------------------------
    // The oracle's errno table, measured (`erl_errno_id`, OTP 29.1.1 / erts-17.1, Linux)
    // -------------------------------------------------------------------------------------------
    //
    // Each row below was **measured**, not recalled: the emulator's own `erl_errno_id` was driven
    // with the numeric code and its answer recorded verbatim (`erl_errno_str.c`'s lookup, i.e. the
    // hardcoded collision list first and then `strerrorname_np`, lowercased). The numeric codes
    // are the platform's (`libc`'s) values on Linux, which is why they are spelled as literals: a
    // row pins *both* the number and the name, so a `libc` bump cannot silently shift one.
    //
    // The measurement harness is the same one `erts/emulator/test/driver_SUITE_data/
    // erl_errno_id_drv.c` uses (`erl_ddll:load_driver` + `erl_errno_id`), driven from a throwaway
    // driver so every code could be asked for individually.
    const ORACLE_ERRNO_ROWS: &[(i32, &str)] = &[
        // The `fork`/`exec`/`pipe`/`access`/file set.
        (7, "e2big"),
        (13, "eacces"),
        (11, "eagain"),
        (9, "ebadf"),
        (16, "ebusy"),
        (10, "echild"),
        (122, "edquot"),
        (17, "eexist"),
        (14, "efault"),
        (27, "efbig"),
        (84, "eilseq"),
        (4, "eintr"),
        (22, "einval"),
        (5, "eio"),
        (21, "eisdir"),
        (40, "eloop"),
        (24, "emfile"),
        (31, "emlink"),
        (36, "enametoolong"),
        (23, "enfile"),
        (19, "enodev"),
        (2, "enoent"),
        (8, "enoexec"),
        (12, "enomem"),
        (28, "enospc"),
        (38, "enosys"),
        (20, "enotdir"),
        (39, "enotempty"),
        (25, "enotty"),
        (6, "enxio"),
        (1, "eperm"),
        (32, "epipe"),
        (30, "erofs"),
        (29, "espipe"),
        (3, "esrch"),
        (110, "etimedout"),
        (26, "etxtbsy"),
        (18, "exdev"),
        // The socket set.
        (98, "eaddrinuse"),
        (99, "eaddrnotavail"),
        (97, "eafnosupport"),
        (114, "ealready"),
        (103, "econnaborted"),
        (111, "econnrefused"),
        (104, "econnreset"),
        (89, "edestaddrreq"),
        (112, "ehostdown"),
        (113, "ehostunreach"),
        (115, "einprogress"),
        (106, "eisconn"),
        (90, "emsgsize"),
        (100, "enetdown"),
        (102, "enetreset"),
        (101, "enetunreach"),
        (105, "enobufs"),
        (92, "enoprotoopt"),
        (107, "enotconn"),
        (88, "enotsock"),
        // `ENOTSUP == EOPNOTSUPP == 95` on Linux; the reference's collision list answers
        // `ENOTSUP`, i.e. `enotsup` — *not* `eopnotsupp`.
        (95, "enotsup"),
        (93, "eprotonosupport"),
        (91, "eprototype"),
    ];

    #[test]
    fn every_row_is_the_name_the_oracle_reports() {
        for (code, name) in ORACLE_ERRNO_ROWS {
            assert_eq!(
                errno_atom(*code),
                Some(*name),
                "errno {code}: the oracle says {name}"
            );
            // `errno_name` is the same source of truth in its string form.
            assert_eq!(errno_name(*code), *name, "errno {code}: errno_name");
        }
        // No row was added without being measured, and none was lost.
        assert_eq!(
            ERRNO_NAMES.len(),
            ORACLE_ERRNO_ROWS.len(),
            "the table and the measured oracle rows disagree in size: {ERRNO_NAMES:?}"
        );
    }

    #[test]
    fn no_code_has_two_spellings_and_every_name_is_lowercase_ascii() {
        for (index, (code, name)) in ERRNO_NAMES.iter().enumerate() {
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "{name} is not the lowercase of a POSIX macro name"
            );
            assert!(
                !name.is_empty(),
                "a row must carry a rendered name (got an empty one)"
            );
            // The same code must not appear twice with a different name: a lookup would then
            // depend on row order (the collision case — `EAGAIN`/`EWOULDBLOCK`,
            // `ENOTSUP`/`EOPNOTSUPP` — has exactly one row).
            for (other_code, other_name) in &ERRNO_NAMES[index + 1..] {
                assert!(
                    other_code != code || other_name == name,
                    "{code} is spelled both {name} and {other_name}"
                );
            }
        }
    }

    #[test]
    fn a_code_the_oracle_cannot_name_keeps_the_generic_errno_n_form() {
        // Measured: `erl_errno_id(0|−1|4711|−4711)` answers `errno_0`, `errno_-1`, `errno_4711`
        // and `errno_-4711` (`ERTS_UNKNOWN_ERRNO_FORMAT`, the same three the emulator's own
        // `erl_errno_id_drv.c` pins with `TEST_ERRNO_UNKNOWN`).
        for code in [0, -1, 4711, -4711] {
            assert_eq!(errno_atom(code), None, "{code} has no POSIX name");
            assert_eq!(errno_name(code), format!("errno_{code}"));
        }
    }

    // -------------------------------------------------------------------------------------------
    // The oracle cross-check: real failures provoked under the system Erlang
    // -------------------------------------------------------------------------------------------

    const ERL_TIMEOUT: Duration = Duration::from_secs(60);

    /// Print a loud skip banner (never a silent pass), with the reason.
    fn loud_skip(reason: &str) {
        println!("================================================================");
        println!("SKIP: {reason}");
        println!("================================================================");
    }

    /// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after a
    /// loud skip message — when `erl` cannot be spawned or does not answer within [`ERL_TIMEOUT`].
    fn erl_stdout(expr: &str) -> Option<String> {
        let code = format!("{expr}, halt(0).");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(
                Command::new("erl")
                    .arg("-noshell")
                    .arg("-eval")
                    .arg(&code)
                    .output(),
            );
        });
        match rx.recv_timeout(ERL_TIMEOUT) {
            Err(_) => {
                loud_skip(&format!(
                    "the `erl` oracle did not answer within {ERL_TIMEOUT:?}"
                ));
                None
            }
            Ok(Err(err)) => {
                loud_skip(&format!(
                    "could not spawn `erl` ({err}); is Erlang on `PATH`?"
                ));
                None
            }
            Ok(Ok(out)) => {
                assert!(
                    out.status.success(),
                    "the oracle expression failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
        }
    }

    /// A throwaway directory for the file-system fixtures the oracle probes.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(stem: &str) -> Scratch {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "beam_io-errno-{stem}-{}-{n}-{nanos}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("nonempty")).expect("create the scratch directory");
            std::fs::write(dir.join("file"), b"x").expect("write the fixture file");
            std::fs::write(dir.join("nonempty").join("child"), b"x")
                .expect("write the nested fixture file");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The errno atoms the oracle really prints, for failures this test provokes.
    ///
    /// This is the live half of the pin above: each entry is a real system call failing inside
    /// the oracle (`{error, Atom}` shapes only — the atom comes from `erl_errno_id`, i.e. the very
    /// table a Rust caller renders through), so the table is compared against observed behaviour
    /// rather than against a memory of the C. The socket rows are the ones the inet driver
    /// reports; the file rows exercise the same table through the `fork`/`exec`/`access` side.
    ///
    /// Deliberately absent: `eacces`/`eperm` (their answer depends on the uid the suite runs as)
    /// and `enotconn`/`enotempty` (a peer `RST` races with the driver's own `closed` atom, and
    /// `rmdir` answers `eexist` on some filesystems). Those rows stay pinned by the measured
    /// table above instead.
    #[test]
    fn the_oracle_reports_the_same_atoms_for_provoked_failures() {
        let scratch = Scratch::new("scenario");
        let Some(dir) = scratch.0.to_str().map(str::to_string) else {
            loud_skip("the scratch path is not valid UTF-8; skipping the oracle cross-check");
            return;
        };
        if dir.contains(['\\', '"']) {
            loud_skip("the scratch path needs escaping; skipping the oracle cross-check");
            return;
        }
        // `Pr` prints one `tag atom` row per probe; a probe that unexpectedly *succeeded* prints
        // `tag none`, which the assertions below reject.
        let expr = format!(
            r#"Pr = fun(T,R) -> case R of {{error,A}} -> io:format("~s ~s~n",[T,A]); _ -> io:format("~s none~n",[T]) end end,
               {{ok,L0}} = gen_tcp:listen(0,[{{active,false}}]), {{ok,P0}} = inet:port(L0), gen_tcp:close(L0),
               Pr("econnrefused", gen_tcp:connect({{127,0,0,1}},P0,[{{active,false}}],500)),
               {{ok,L1}} = gen_tcp:listen(0,[{{active,false}}]), {{ok,P1}} = inet:port(L1),
               Pr("eaddrinuse", gen_tcp:listen(P1,[{{active,false}},{{reuseaddr,false}}])),
               gen_tcp:close(L1),
               Pr("eaddrnotavail", gen_tcp:listen(0,[{{active,false}},{{ip,{{10,255,255,254}}}}])),
               {{ok,U0}} = gen_udp:open(0,[binary]),
               Pr("emsgsize", gen_udp:send(U0,{{127,0,0,1}},9,binary:copy(<<"x">>,70000))),
               Pr("einval", gen_udp:recv(U0,0,0)),
               gen_udp:close(U0),
               Pr("enoent", file:read_file("{dir}/missing")),
               Pr("eexist", file:make_dir("{dir}")),
               Pr("eisdir", file:read_file("{dir}")),
               Pr("enotdir", file:read_file("{dir}/file/child")),
               Pr("enametoolong", file:read_file("{dir}/" ++ lists:duplicate(5000,$a))),
               ok"#
        );
        let Some(stdout) = erl_stdout(&expr) else {
            return;
        };

        let rows: Vec<(String, String)> = stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.to_string(), fields.next()?.to_string()))
            })
            .collect();

        let expected: &[(&str, i32)] = &[
            ("econnrefused", libc::ECONNREFUSED),
            ("eaddrinuse", libc::EADDRINUSE),
            ("eaddrnotavail", libc::EADDRNOTAVAIL),
            ("emsgsize", libc::EMSGSIZE),
            ("einval", libc::EINVAL),
            ("enoent", libc::ENOENT),
            ("eexist", libc::EEXIST),
            ("eisdir", libc::EISDIR),
            ("enotdir", libc::ENOTDIR),
            ("enametoolong", libc::ENAMETOOLONG),
        ];
        for (tag, code) in expected {
            let atom = rows
                .iter()
                .find(|(row_tag, _)| row_tag == tag)
                .map(|(_, atom)| atom.as_str())
                .unwrap_or_else(|| {
                    panic!("the oracle printed no `{tag}` row (it answered: {stdout})")
                });
            assert_eq!(
                Some(atom),
                errno_atom(*code),
                "the oracle answered {atom} for {tag}; our table says {:?}",
                errno_atom(*code)
            );
        }
    }
}
