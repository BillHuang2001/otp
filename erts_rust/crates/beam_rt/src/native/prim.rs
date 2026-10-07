//! The runtime-native `prim_file:*` floor (contract **A.7**, the `prim_*` family).
//!
//! `prim_file` is a **preloaded** module: the file-system primitives `file.erl`
//! delegates to. In the oracle its BIFs are the C `prim_file_nif.c` NIFs
//! (`../../erts/emulator/nifs/{common,unix}/prim_file_nif.c`), registered by
//! `erts/preloaded/src/prim_file.erl`'s `-nifs()` table; without a host for that NIF
//! this runtime's preloaded `prim_file.beam` calls `erlang:nif_error(undef)` and the
//! caller gets an honest raise.
//!
//! This module serves the **smallest subset that is genuinely a runtime primitive** and
//! whose whole observable contract is pinned byte-for-byte against the oracle, backed by
//! the host's own file-system calls (`std::fs`/`std::env`) — "primitive" is exactly what
//! this layer is for. Everything else returns [`None`] from
//! [`RuntimeContext::prim_file_call`], so the call falls through to the preloaded
//! `.beam` — never a wrong value, never a fabricated success.
//!
//! # BIFs served (each verified against the `erl` oracle, OTP 29.1.1 / erts-17.1)
//!
//! | BIF | Ground truth | Probe ⇒ result |
//! |---|---|---|
//! | `prim_file:get_cwd/0` | `prim_file.erl:787` → `get_cwd_nif` → `efile_get_cwd` (`unix_prim_file.c:1050`, `getcwd(3)`), decoded by `internal_native2name` | `{ok,"/home/…"}` (the cwd as a **string**) |
//! | `prim_file:get_cwd/1` | `prim_file.erl:771-778` (pure Erlang clauses) | `[]`→`{error,badarg}`; `"C:"`→`{error,enotsup}` (Unix `efile_get_device_cwd` is a constant `ENOTSUP`, `unix_prim_file.c:1042`); `"AB:"`/`":"`/`"ab"`/`[1,2]`/`[65\|foo]`→`{error,einval}`; `<<"C:">>`/`floppy`→`{error,badarg}` |
//! | `prim_file:read_file/1` | `prim_file.erl:564` → `read_file_nif`; the path through `encode_path` = `internal_name2native` (`erl_unicode.c:2629`) | `{ok,<<"…">>}`; a missing file→`{error,enoent}`; a directory→`{error,eisdir}`; a non-directory component→`{error,enotdir}`; a symlink loop→`{error,eloop}`; a bad path (`foo`, `1`, `{raw,_}`, `<<…,0,…>>`, a NUL code point, an out-of-range/surrogate code point, a binary *inside* an iolist, an improper list)→`{error,badarg}` |
//!
//! # The path contract (`encode_path/1` → `internal_name2native/1`)
//!
//! A path is a **binary** (its bytes, verbatim: no NUL allowed) or an **iolist** of code
//! points, flattened—nested lists included—and encoded as UTF-8 (the native filename
//! encoding on Unix). An atom is `badarg` (`erl_unicode.c:2645`); a binary *element* of
//! an iolist is `badarg` (`erts_native_filename_need` never flattens one: `:2374-2383`);
//! so is a NUL, an improper tail, a non-integer element, and any code point that is not a
//! Unicode scalar value (`:2306-2354`).
//!
//! # Not served
//!
//! Every other `prim_file` entry (`open`/`read`/`write`/`position`/`stat`/`list_dir`/…)
//! so the preloaded `.beam` still raises. `list_dir/1` is deliberately **not** claimed:
//! its entries come back in `readdir(3)` order, which the oracle does not sort and this
//! runtime cannot pin. `prim_zip:splitter/3` and the other `prim_*` modules
//! (`prim_buffer`, `prim_eval`, `prim_inet`/`prim_socket`/`prim_net`, `erl_prim_loader`)
//! are untouched — see `CONTEXT.md` for the machinery each needs.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, is_nil, is_small, signed_value, Eterm};

use super::RuntimeContext;

impl RuntimeContext<'_> {
    /// The `prim_file:*` half of the native floor.
    ///
    /// Returns `None` when `(function, arity)` is not one this module serves — the
    /// caller then falls through to the `beam_bifs` registry and then to the preloaded
    /// `prim_file.beam` (whose `-nifs()` stubs raise `undef`).
    pub(super) fn prim_file_call(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (function, arity) {
            ("get_cwd", 0) => Some(self.prim_get_cwd_0()),
            ("get_cwd", 1) => Some(self.prim_get_cwd_1(args)),
            ("read_file", 1) => Some(self.prim_read_file(args)),
            _ => None,
        }
    }

    // -- prim_file:get_cwd/0,1 ------------------------------------------------

    /// `prim_file:get_cwd/0` — the process's current working directory as
    /// `{ok, Cwd}` (`prim_file.erl:787-793`).
    ///
    /// The C answers the **raw** path as a byte binary and the Erlang wrapper decodes it
    /// with `decode_path/1` = `internal_native2name/1`; on a Unix UTF-8 native encoding
    /// that is the path's code points as a **string** (a char list) — the oracle's
    /// `{ok,"/home/…"}`. A native path that is not valid UTF-8 is the one case where the
    /// C's decoder answers `{error,warning}` (`erl_unicode.c:2774-2789`, the default
    /// warning type) instead of a string, so this reports the same.
    fn prim_get_cwd_0(&mut self) -> Result<Eterm> {
        match std::env::current_dir() {
            Ok(directory) => {
                let cwd = match directory.to_str() {
                    Some(text) => self.charlist(text)?,
                    None => {
                        let error = self.atom("error")?;
                        let warning = self.atom("warning")?;
                        terms::make_tuple(self, &[error, warning])?
                    }
                };
                self.ok_tuple(cwd)
            }
            Err(error) => self.error_tuple_for(&error),
        }
    }

    /// `prim_file:get_cwd/1` — the Windows drive-letter form (`prim_file.erl:771-778`).
    ///
    /// The three clauses are pure Erlang, so they are reproduced verbatim: a two-element
    /// `[Letter, $:]` list whose letter is `A`–`Z`/`a`–`z` goes to `get_device_cwd_nif`,
    /// which on Unix is a constant `ENOTSUP` (`unix_prim_file.c:1042-1048`); every other
    /// **cons cell** is `{error,einval}` (including an improper list — the `[_|_]`
    /// pattern matches one); anything else (`[]`, an atom, an integer, a binary) is
    /// `{error,badarg}`.
    ///
    /// The C's only `badarg` here (`enif_get_int` on the device index) is unreachable
    /// because the letter guard already restricted the list to two small integers.
    fn prim_get_cwd_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        if is_list(argument) {
            if let Some((letter, colon)) = two_element_list(argument) {
                if is_small(colon) && signed_value(colon) == i64::from(b':') {
                    if is_small(letter) {
                        let letter = signed_value(letter);
                        if (i64::from(b'A')..=i64::from(b'Z')).contains(&letter)
                            || (i64::from(b'a')..=i64::from(b'z')).contains(&letter)
                        {
                            return self.error_tuple("enotsup");
                        }
                    }
                }
            }
            return self.error_tuple("einval");
        }
        self.error_tuple("badarg")
    }

    // -- prim_file:read_file/1 ------------------------------------------------

    /// `prim_file:read_file/1` — the whole file as `{ok, Binary}`, or `{error, Reason}`
    /// (`prim_file.erl:564-570`).
    ///
    /// The Erlang wrapper runs `read_file_nif(encode_path(Filename))` inside
    /// `try … catch error:badarg -> {error, badarg} end`, so a **bad path** is the
    /// *value* `{error,badarg}`, never a raise.
    fn prim_read_file(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        let Some(path) = self.encode_path(argument) else {
            return self.error_tuple("badarg");
        };
        match fs::read(std::ffi::OsStr::from_bytes(&path)) {
            // The contents are built on the runtime's pointer-stable `-eval` heap so the
            // returned term outlives this call (`beam_codec::term::make_binary` is the
            // same on-heap `ErlHeapBits` writer the `-eval` binary surface uses, and —
            // unlike `beam_engine::terms::make_heap_binary` — it has no 64-byte limit).
            Ok(bytes) => {
                let contents = beam_codec::term::make_binary(self.eval_heap(), &bytes)?;
                self.ok_tuple(contents)
            }
            Err(error) => self.error_tuple_for(&error),
        }
    }

    /// The C's `encode_path/1` = `prim_file:internal_name2native/1`
    /// (`erl_unicode.c:2629-2733`): the accepted path forms as bytes, or `None` for the
    /// C's `badarg`.
    fn encode_path(&self, term: Eterm) -> Option<Vec<u8>> {
        // `prim_file` explicitly does not accept atoms (`erl_unicode.c:2641-2647`).
        if is_atom(term) {
            return None;
        }
        if let Ok(bytes) = terms::binary_bytes(term) {
            // A NUL in the middle of a filename is rejected (`:2666-2668`). A
            // non-byte-aligned bitstring is not a binary, so it falls to the walk below,
            // which rejects it exactly as `erts_get_aligned_binary_bytes` does.
            if bytes.contains(&0) {
                return None;
            }
            return Some(bytes.to_vec());
        }
        let mut path = Vec::new();
        collect_path(term, &mut path).ok()?;
        Some(path)
    }

    /// `{ok, Value}`.
    fn ok_tuple(&mut self, value: Eterm) -> Result<Eterm> {
        let ok = self.atom("ok")?;
        terms::make_tuple(self, &[ok, value])
    }

    /// `{error, Reason}` for the literal atom `reason`.
    fn error_tuple(&mut self, reason: &str) -> Result<Eterm> {
        let error = self.atom("error")?;
        let reason = self.atom(reason)?;
        terms::make_tuple(self, &[error, reason])
    }

    /// `{error, Reason}` with `Reason` derived from a host I/O failure — the C's
    /// `posix_error_to_tuple(erl_errno_id(errno))`.
    fn error_tuple_for(&mut self, error: &io::Error) -> Result<Eterm> {
        let reason = errno_atom_name(error);
        self.error_tuple(&reason)
    }
}

/// The `[First, Second]` pattern of `prim_file:get_cwd/1` (`prim_file.erl:771-778`):
/// `Some` only for a **proper** two-element list, so the caller's `[_|_]` clause covers
/// every other cons cell.
fn two_element_list(term: Eterm) -> Option<(Eterm, Eterm)> {
    if !is_list(term) {
        return None;
    }
    let first = terms::cons_head(term).ok()?;
    let tail = terms::cons_tail(term).ok()?;
    if !is_list(tail) {
        return None;
    }
    let second = terms::cons_head(tail).ok()?;
    if !is_nil(terms::cons_tail(tail).ok()?) {
        return None;
    }
    Some((first, second))
}

/// The C's iolist walk of `erts_native_filename_need` (`erl_unicode.c:2222-2398`)
/// combined with its write half (`erts_native_filename_put`, `:2400`): appends the
/// UTF-8 encoding of `term`'s code points to `path`.
///
/// `Err(BeamError::BadArg)` is the C's `-1` → `badarg`: a NUL code point, a code point
/// that is not a Unicode scalar value, a non-integer element, a nested **binary**
/// (`erts_native_filename_need` never flattens one), or a non-list / improper tail.
///
/// The walk is iterative with an explicit stack of pending list tails, mirroring the
/// C's `ESTACK`, so an arbitrarily deep nesting cannot exhaust the Rust stack.
fn collect_path(term: Eterm, path: &mut Vec<u8>) -> Result<()> {
    let mut pending: Vec<Eterm> = Vec::new();
    let mut current = term;
    let mut buffer = [0u8; 4];
    loop {
        if is_nil(current) {
            // "ignore empty lists" — resume the enclosing list, or finish
            // (`erl_unicode.c:2296-2299`).
            match pending.pop() {
                Some(rest) => {
                    current = rest;
                    continue;
                }
                None => return Ok(()),
            }
        }
        if !is_list(current) {
            // An improper tail, or a term that is not a list at all (`:2287-2290`).
            return Err(BeamError::BadArg);
        }
        let head = terms::cons_head(current)?;
        let tail = terms::cons_tail(current)?;
        if is_small(head) {
            let code = signed_value(head);
            if code == 0 {
                return Err(BeamError::BadArg);
            }
            let code = u32::try_from(code).map_err(|_| BeamError::BadArg)?;
            let character = char::from_u32(code).ok_or(BeamError::BadArg)?;
            path.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        } else if is_list(head) {
            // Descend into the sublist, remembering where this one resumes
            // (`:2374-2379`).
            if !is_nil(tail) {
                pending.push(tail);
            }
            current = head;
            continue;
        } else if !is_nil(head) {
            // A binary, a float, a tuple, … — `is_nil(obj)` skips, everything else here
            // is the C's `-1` (`:2367-2383`).
            return Err(BeamError::BadArg);
        }
        current = tail;
    }
}

/// `errno` → the atom the C's `erl_errno_id/1` produces (`erl_errno_str.c:1229`) : the
/// POSIX name lower-cased (`tolower_copy_estring`, `:107`).
///
/// The table covers every errno a file-system operation can raise. An errno outside it
/// falls back to the C's own format for an errno it has no name for
/// (`ERTS_UNKNOWN_ERRNO_FORMAT`, `errno_<n>`: `:47`), so the answer is still the C's.
fn errno_atom_name(error: &io::Error) -> String {
    // `fs::read`/`current_dir` are syscall-only, so every error carries an errno — the
    // C's exact input. An errno-less error has no C counterpart: report the C's
    // unknown-errno form for `0` rather than invent an atom.
    let code = error.raw_os_error().unwrap_or(0);
    match ERRNO_NAMES.iter().find(|&&(errno, _)| errno == code) {
        Some(&(_, name)) => name.to_string(),
        None => format!("errno_{code}"),
    }
}

/// `errno` → the C's lower-cased name, for the errnos a file operation can produce.
const ERRNO_NAMES: &[(i32, &str)] = &[
    (libc::EPERM, "eperm"),
    (libc::ENOENT, "enoent"),
    (libc::ESRCH, "esrch"),
    (libc::EINTR, "eintr"),
    (libc::EIO, "eio"),
    (libc::ENXIO, "enxio"),
    (libc::E2BIG, "e2big"),
    (libc::ENOEXEC, "enoexec"),
    (libc::EBADF, "ebadf"),
    (libc::ECHILD, "echild"),
    (libc::EAGAIN, "eagain"),
    (libc::ENOMEM, "enomem"),
    (libc::EACCES, "eacces"),
    (libc::EFAULT, "efault"),
    (libc::EBUSY, "ebusy"),
    (libc::EEXIST, "eexist"),
    (libc::EXDEV, "exdev"),
    (libc::ENODEV, "enodev"),
    (libc::ENOTDIR, "enotdir"),
    (libc::EISDIR, "eisdir"),
    (libc::EINVAL, "einval"),
    (libc::ENFILE, "enfile"),
    (libc::EMFILE, "emfile"),
    (libc::ENOTTY, "enotty"),
    (libc::ETXTBSY, "etxtbsy"),
    (libc::EFBIG, "efbig"),
    (libc::ENOSPC, "enospc"),
    (libc::ESPIPE, "espipe"),
    (libc::EROFS, "erofs"),
    (libc::EMLINK, "emlink"),
    (libc::EPIPE, "epipe"),
    (libc::ERANGE, "erange"),
    (libc::EDEADLK, "edeadlk"),
    (libc::ENAMETOOLONG, "enametoolong"),
    (libc::ENOLCK, "enolck"),
    (libc::ENOSYS, "enosys"),
    (libc::ENOTEMPTY, "enotempty"),
    (libc::ELOOP, "eloop"),
    (libc::EOVERFLOW, "eoverflow"),
    (libc::EILSEQ, "eilseq"),
    (libc::ESTALE, "estale"),
    (libc::EDQUOT, "edquot"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::RuntimeContext;
    use crate::native::test_support::{atom, call, chars, runtime, string_of};
    use crate::runtime::Runtime;
    use beam_bifs::NullSink;
    use beam_term::{make_small, NIL};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -- fixtures -------------------------------------------------------------

    /// The sequence that keeps each test's scratch directory unique even when the
    /// module's tests run in parallel inside one process.
    static SCRATCH_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// A fresh, writable scratch directory.
    fn scratch_dir(stem: &str) -> PathBuf {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_prim_{stem}_{}_{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
        dir
    }

    /// Builds a term on the runtime's pointer-stable arena through a temporary context,
    /// so it stays valid for the [`call`] that reads it.
    fn heap_term(
        runtime: &mut Runtime,
        build: impl for<'a> FnOnce(&mut RuntimeContext<'a>) -> Result<Eterm>,
    ) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        build(&mut context).expect("a heap term")
    }

    /// `bytes` as an Erlang binary on the runtime's pointer-stable arena.
    ///
    /// Built by the same `beam_codec::term::make_binary` the `prim_file:read_file/1`
    /// implementation uses, so a scratch path longer than the 64-byte on-heap limit
    /// (`beam_engine::terms::make_heap_binary`'s) is still representable.
    fn binary(runtime: &mut Runtime, bytes: &[u8]) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        beam_codec::term::make_binary(context.eval_heap(), bytes).expect("a binary")
    }

    /// `term` as the atom name it is (the tests assert names, never indices).
    fn atom_name(runtime: &Runtime, term: Eterm) -> String {
        runtime
            .atom_name_of(term)
            .unwrap_or_else(|| panic!("{term:?} is not an atom"))
            .to_string()
    }

    /// The `ok` value of an `{ok, Value}` answer.
    fn ok_value(runtime: &Runtime, answer: Eterm) -> Eterm {
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        assert_eq!(atom_name(runtime, element(answer, 0)), "ok");
        element(answer, 1)
    }

    /// The reason atom of an `{error, Reason}` answer.
    fn error_reason(runtime: &Runtime, answer: Eterm) -> String {
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        assert_eq!(atom_name(runtime, element(answer, 0)), "error");
        atom_name(runtime, element(answer, 1))
    }

    /// The `index`-th element of a tuple.
    fn element(answer: Eterm, index: usize) -> Eterm {
        terms::tuple_element(answer, index).expect("an element")
    }

    /// `prim_file:function(args)` through the runtime's dispatch.
    fn prim_file(runtime: &mut Runtime, function: &str, args: &[Eterm]) -> Eterm {
        call(runtime, "prim_file", function, args).expect("a prim_file answer")
    }

    // -- prim_file:get_cwd/0 --------------------------------------------------

    /// Oracle: `erl -noshell -eval 'io:format("~p~n",[prim_file:get_cwd()]), halt().'`
    /// ⇒ `{ok,"/home/bill/Source/otp/.genesis/workers/worker_T2_A1033"}` — the cwd as a
    /// **string**. The host's `std::env::current_dir()` is the same `getcwd(3)` value.
    #[test]
    fn get_cwd_0_returns_the_working_directory_as_a_string() {
        let mut runtime = runtime();
        let answer = prim_file(&mut runtime, "get_cwd", &[]);
        let cwd = ok_value(&runtime, answer);
        let expected = std::env::current_dir().expect("a cwd");
        assert_eq!(string_of(cwd), expected.to_str().expect("a UTF-8 cwd"));
    }

    // -- prim_file:get_cwd/1 --------------------------------------------------

    /// Oracle (OTP 29.1.1): `get_cwd([])`⇒`{error,badarg}`; `get_cwd("C:")`/`("a:")`
    /// ⇒`{error,enotsup}`; `get_cwd("AB:")`/`(":")`/`("ab")`/`([1,2])`/`([65|foo])`
    /// ⇒`{error,einval}`; `get_cwd(<<"C:">>)`/`(floppy)`⇒`{error,badarg}`.
    #[test]
    fn get_cwd_1_matches_the_oracle() {
        let mut runtime = runtime();
        for (argument, expected) in [
            (NIL, "badarg"),
            (chars(&mut runtime, "C:"), "enotsup"),
            (chars(&mut runtime, "a:"), "enotsup"),
            (chars(&mut runtime, "AB:"), "einval"),
            (chars(&mut runtime, ":"), "einval"),
            (chars(&mut runtime, "ab"), "einval"),
            (chars(&mut runtime, "@:"), "einval"),
            (chars(&mut runtime, "{:"), "einval"),
            (chars(&mut runtime, "A"), "einval"),
        ] {
            let answer = prim_file(&mut runtime, "get_cwd", &[argument]);
            assert_eq!(error_reason(&runtime, answer), expected, "{argument:?}");
        }
        // `[1,2]` (a two-element list that is not `[Letter, $:]`).
        let pair = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(1), make_small(2)]));
        let answer = prim_file(&mut runtime, "get_cwd", &[pair]);
        assert_eq!(error_reason(&runtime, answer), "einval");
        // An improper list still matches the C's `[_|_]` clause.
        let improper = heap_term(&mut runtime, |ctx| {
            let tail = ctx.atom("foo")?;
            terms::make_cons(ctx, make_small(65), tail)
        });
        let answer = prim_file(&mut runtime, "get_cwd", &[improper]);
        assert_eq!(error_reason(&runtime, answer), "einval");
        // A binary and a non-list non-nil term are `badarg`.
        let binary_path = binary(&mut runtime, b"C:");
        let answer = prim_file(&mut runtime, "get_cwd", &[binary_path]);
        assert_eq!(error_reason(&runtime, answer), "badarg");
        let floppy = atom(&mut runtime, "floppy");
        let answer = prim_file(&mut runtime, "get_cwd", &[floppy]);
        assert_eq!(error_reason(&runtime, answer), "badarg");
    }

    // -- prim_file:read_file/1 ------------------------------------------------

    /// Oracle: `prim_file:read_file(<<"/tmp/…/hello.txt">>)` ⇒ `{ok,<<"hello">>}`;
    /// the same path as a char list (`"/tmp/…/hello.txt"`) ⇒ `{ok,<<"hello">>}`; an
    /// iolist (`"[…/tmp", "/hello.txt"]`) ⇒ `{ok,<<"hello">>}`.
    #[test]
    fn read_file_returns_the_contents_as_a_binary() {
        let mut runtime = runtime();
        let dir = scratch_dir("contents");
        let path = dir.join("hello.txt");
        std::fs::write(&path, b"hello").expect("the file is writable");
        let path = path.to_str().expect("a UTF-8 path");

        for argument in [
            binary(&mut runtime, path.as_bytes()),
            chars(&mut runtime, path),
            {
                let (head, tail) = (&path[..4], &path[4..]);
                let head = chars(&mut runtime, head);
                let tail = chars(&mut runtime, tail);
                heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[head, tail]))
            },
        ] {
            let answer = prim_file(&mut runtime, "read_file", &[argument]);
            let contents = ok_value(&runtime, answer);
            assert_eq!(
                terms::binary_bytes(contents).expect("a binary"),
                b"hello",
                "{argument:?}"
            );
        }
    }

    /// Oracle: reading a **missing** file ⇒ `{error,enoent}`; a **directory**
    /// ⇒ `{error,eisdir}`; a path through a non-directory ⇒ `{error,enotdir}`.
    #[test]
    fn read_file_maps_file_system_failures_to_errno_atoms() {
        let mut runtime = runtime();
        let dir = scratch_dir("failures");
        let file = dir.join("plain.txt");
        std::fs::write(&file, b"x").expect("the file is writable");

        let cases = [
            (dir.join("missing.txt"), "enoent"),
            (dir.clone(), "eisdir"),
            (file.join("below"), "enotdir"),
        ];
        for (path, expected) in cases {
            let path = binary(&mut runtime, path.to_str().expect("a UTF-8 path").as_bytes());
            let answer = prim_file(&mut runtime, "read_file", &[path]);
            assert_eq!(error_reason(&runtime, answer), expected, "{expected}");
        }
    }

    /// Oracle: a path that `encode_path/1` rejects is the **value** `{error,badarg}`
    /// (`prim_file.erl:566-570`) — never a raise. Probes: `read_file(foo)`,
    /// `read_file(1)`, `read_file({raw,<<"x">>})`, `read_file([0])`,
    /// `read_file([-1])`, `read_file([16#110000])`, `read_file([16#D800])`,
    /// `read_file([1|2])`, `read_file([<<"/tmp">>])`, `read_file(<<"/tmp/a",0,"b">>)`.
    #[test]
    fn read_file_of_a_bad_path_is_the_badarg_value() {
        let mut runtime = runtime();
        let nul_binary = binary(&mut runtime, b"/tmp/a\0b");
        let nested_binary = heap_term(&mut runtime, |ctx| {
            let inner = terms::make_heap_binary(ctx, b"/tmp")?;
            terms::make_list(ctx, &[inner])
        });
        let improper = heap_term(&mut runtime, |ctx| {
            terms::make_cons(ctx, make_small(1), make_small(2))
        });
        let tuple = heap_term(&mut runtime, |ctx| {
            let raw = ctx.atom("raw")?;
            let inner = terms::make_heap_binary(ctx, b"x")?;
            terms::make_tuple(ctx, &[raw, inner])
        });
        let nul_codepoint = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(0)]));
        let negative = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(-1)]));
        let too_large =
            heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(0x11_0000)]));
        let surrogate = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(0xD800)]));
        let bad_paths = [
            atom(&mut runtime, "foo"),
            make_small(1),
            tuple,
            nul_codepoint,
            negative,
            too_large,
            surrogate,
            improper,
            nested_binary,
            nul_binary,
        ];
        for argument in bad_paths {
            let answer = prim_file(&mut runtime, "read_file", &[argument]);
            assert_eq!(error_reason(&runtime, answer), "badarg", "{argument:?}");
        }
    }

    /// The empty path (`<<>>`, `[]`, `[[]]`) is *valid* and reaches the file system as
    /// the empty name — the oracle answers `{error,enoent}` for all three.
    #[test]
    fn read_file_of_an_empty_path_reaches_the_file_system() {
        let mut runtime = runtime();
        let empty_binary = binary(&mut runtime, b"");
        let nested_empty = heap_term(&mut runtime, |ctx| {
            let inner = terms::make_list(ctx, &[])?;
            terms::make_list(ctx, &[inner])
        });
        for argument in [empty_binary, NIL, nested_empty] {
            let answer = prim_file(&mut runtime, "read_file", &[argument]);
            assert_eq!(error_reason(&runtime, answer), "enoent", "{argument:?}");
        }
    }

    /// A file past the 64-byte on-heap limit still round-trips: the contents are built
    /// by `beam_codec::term::make_binary`, which — unlike
    /// `beam_engine::terms::make_heap_binary` — has no size limit.
    #[test]
    fn read_file_returns_a_file_larger_than_one_word_block() {
        let mut runtime = runtime();
        let dir = scratch_dir("large");
        let path = dir.join("large.bin");
        let contents: Vec<u8> = (0..=255u8).cycle().take(200).collect();
        std::fs::write(&path, &contents).expect("the file is writable");
        let path = binary(&mut runtime, path.to_str().expect("a UTF-8 path").as_bytes());
        let answer = prim_file(&mut runtime, "read_file", &[path]);
        let read = ok_value(&runtime, answer);
        assert_eq!(terms::binary_bytes(read).expect("a binary"), &contents[..]);
    }

    // -- the errno table ------------------------------------------------------

    /// The reasons the oracle produces for the failures above map to the C's names
    /// (`erl_errno_str.c:1229` + `errno_name`), and an unknown errno is the C's
    /// `errno_<n>` form (`ERTS_UNKNOWN_ERRNO_FORMAT`, `:47`).
    #[test]
    fn errno_names_follow_the_c() {
        for (errno, expected) in [
            (libc::ENOENT, "enoent"),
            (libc::EISDIR, "eisdir"),
            (libc::EACCES, "eacces"),
            (libc::ENOTDIR, "enotdir"),
            (libc::ELOOP, "eloop"),
            (libc::ENAMETOOLONG, "enametoolong"),
        ] {
            let error = io::Error::from_raw_os_error(errno);
            assert_eq!(errno_atom_name(&error), expected);
        }
        assert_eq!(
            errno_atom_name(&io::Error::from_raw_os_error(9_999)),
            "errno_9999"
        );
    }
}
