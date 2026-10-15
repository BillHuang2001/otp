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

//! The **path-mutating** `code:*` runtime-native floor entries: `code:add_patha/1,2`,
//! `code:add_pathz/1,2`, `code:add_path/1,2` and `code:del_path/1`.
//!
//! # Why these are floor entries and not `code.erl` bodies
//!
//! The wrapper bodies in `lib/kernel/src/code.erl` are *client* code: each one sends a request
//! to the **live `code_server` process**, which keeps its own copy of the code path. A booted
//! `beam_rt` really does have that process (`whereis(code_server)` is a pid on every
//! boot-driven route), so entering a body lands the directory in the *code server's* copy
//! while every path **reader** this runtime serves — `code:get_path/0`, `code:load_file/1`,
//! `code:which/1`, `code:get_object_code/1`, `code:ensure_loaded/1`, `code:where_is_file/1`
//! and every module lookup — reads [`CodeServer`](crate::CodeServer)'s. The two stores were
//! therefore incoherent, and the observable defect was
//! `code:add_patha(Dir), code:load_file(M)` ⇒ `{error, nofile}` even though
//! `lists:member(Dir, code:get_path())` (answered by that same process) said `true`.
//!
//! The floor owns these keys so there is **one** authoritative store. The bodies below are the
//! oracle's own semantics, taken from the pair
//! `lib/kernel/src/code.erl` (the guard + normalisation) / `lib/kernel/src/code_server.erl`
//! (the path edit), and every shape is oracle-cross-checked in this module's tests.
//!
//! # The oracle's shape
//!
//! * `code.erl:1069-1130` — `add_patha/1,2`, `add_pathz/1,2` and `add_path/1,2` all guard
//!   `is_list(Dir), ?is_cache(Cache)` and then hand a **normalised** directory to the code
//!   server (`{add_path, first | last, Dir, Cache}`); `add_path/1,2` is `add_pathz`.
//! * `code_server.erl:730-761` — `check_path/1` decides `{error, bad_directory}`, and
//!   `do_add/6` performs the edit: `first` **moves** an existing entry to the head,
//!   `last` leaves an existing entry exactly where it is (append-if-absent).
//! * `code_server.erl:884-912` — `del_path/1` matches either the directory as written or its
//!   `get_name/1` application name (the backwards-compatible `del_path(<app>)` form).

use std::path::Path;

use beam_common::{BeamError, Result};
use beam_term::{is_list, is_nil, Eterm};

use crate::codeserver::normalize_dir;
use crate::context::RuntimeContext;

impl RuntimeContext<'_> {
    /// `code:add_patha/1,2`: **prepend** `Dir` to the code path (the oracle's
    /// `{add_path, first, Dir, Cache}` request).
    ///
    /// A directory already on the path is **moved** to the head — `code_server.erl`'s
    /// `do_add(first, …)` runs `lists:keytake(Dir, 1, Path0)` before consing the new entry on,
    /// so the count stays `1` and the head is the added directory (oracle-verified).
    ///
    /// # Errors
    ///
    /// A bare-atom `function_clause` raise when the `code.erl` guard fails — a `Dir` that is
    /// not a list, or a `Cache` that is neither `cache` nor `nocache` (both oracle-verified:
    /// `code:add_patha(foo)`, `code:add_patha(42)` and `code:add_patha(Dir, bogus)` all raise
    /// `function_clause`).
    pub(crate) fn code_add_patha(&mut self, args: &[Eterm]) -> Result<Eterm> {
        self.code_add_path(args, true)
    }

    /// `code:add_pathz/1,2` — and `code:add_path/1,2`, which `code.erl:1075-1077` folds into
    /// it — **append** `Dir` to the code path (the oracle's `{add_path, last, Dir, Cache}`).
    ///
    /// A directory already on the path keeps its **existing position**: `do_add(last, …)` runs
    /// `lists:keyreplace/4`, which replaces the pair in place (oracle-verified: `add_pathz`
    /// of a directory already at the head leaves it at the head).
    ///
    /// # Errors
    ///
    /// The same `function_clause` guard as [`RuntimeContext::code_add_patha`].
    pub(crate) fn code_add_pathz(&mut self, args: &[Eterm]) -> Result<Eterm> {
        self.code_add_path(args, false)
    }

    /// `code:del_path/1`: remove `Name`'s directory from the code path, answering `true` when
    /// one was removed and `false` when none was.
    ///
    /// `Name` is either a complete directory or — the documented backwards-compatibility form
    /// — the atom/string application name a directory is known by
    /// (`code_server.erl`'s `get_name/1`: for `<…>/<app>-<vsn>/ebin` the `app`, truncated at
    /// the first `-`). The directory itself is matched first, the name second, in one pass over
    /// the path.
    ///
    /// # Errors
    ///
    /// `code.erl:1209`'s guard — `del_path(Name) when is_list(Name); is_atom(Name)` — so a
    /// non-list/non-atom argument raises the bare-atom `function_clause` the oracle raises for
    /// `code:del_path(42)`.
    pub(crate) fn code_del_path(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        let name = match self.atom_name(argument) {
            Some(name) => name,
            None => match self.charlist_string(argument) {
                Some(name) => name,
                // A list that is not a proper char list: `code_server.erl`'s
                // `catch filename:join([to_list(Name)])` cannot name a directory either, and
                // the oracle answers its `{error, bad_name}`. This floor has no `NameDb`, so
                // the value is settled by the same `function_clause` guard as a non-list.
                None => return Err(crate::native::prim::function_clause()),
            },
        };
        let wanted = normalize_dir(&name);
        let removed = self.runtime.code_server.remove_path_where(|entry| {
            let entry = entry.to_string_lossy();
            entry == wanted || directory_name(&entry) == wanted
        });
        self.atom(if removed { "true" } else { "false" })
    }

    /// The shared body of the three `add_path*` entries: the guard, the normalisation, the
    /// directory check and the `do_add(first | last, …)` edit.
    ///
    /// `args` is the entry's whole argument list, so the optional second `Cache` argument is
    /// simply absent for the `/1` forms.
    fn code_add_path(&mut self, args: &[Eterm], first: bool) -> Result<Eterm> {
        let dir = *args.first().ok_or(BeamError::BadArg)?;
        // `code.erl:1077,1099,1123`'s guard `is_list(Dir), ?is_cache(Cache)`; `is_nil` covers
        // the empty list, which `is_list/1` accepts. `code:add_patha("")` normalises to the
        // empty string and then fails the directory check, so it answers `{error,
        // bad_directory}` on both runtimes.
        if !is_list(dir) && !is_nil(dir) {
            return Err(crate::native::prim::function_clause());
        }
        if let Some(cache) = args.get(1).copied() {
            if !self.is_cache(cache) {
                return Err(crate::native::prim::function_clause());
            }
        }
        let Some(name) = self.charlist_string(dir) else {
            // `normalize_paths/3`'s `int_list/1` test (`code.erl:1052-1061`) has no clause for
            // a list that is not a proper integer list, so the oracle's `normalize_paths/3`
            // answers `{error, []}` and the caller's `{_, [Normalized]} = …` match fails with
            // `badmatch`. This floor reports the same class of raise with the guard's own
            // reason (`function_clause`) rather than inventing a `{badmatch, _}` shape.
            return Err(crate::native::prim::function_clause());
        };
        let normalized = normalize_dir(&name);
        match self.check_path(&normalized) {
            Some(dir) => {
                if first {
                    self.runtime.code_server.add_path_first(dir);
                } else {
                    self.runtime.code_server.add_path(dir);
                }
                self.atom("true")
            }
            None => self.error_pair("bad_directory"),
        }
    }

    /// `?is_cache(T)` (`code.erl:457`): `T =:= cache orelse T =:= nocache`.
    ///
    /// The value only decides whether the oracle's code server caches a directory's contents
    /// on its first traversal. This runtime's code path is a plain ordered list with no such
    /// cache, so both values are accepted and neither can change an observable answer — the
    /// oracle's `Cache` map is not part of `code:get_path/0` either. A value that is neither is
    /// rejected by the caller, exactly as the guard rejects it.
    fn is_cache(&self, term: Eterm) -> bool {
        matches!(self.atom_name(term).as_deref(), Some("cache" | "nocache"))
    }

    /// `code_server.erl`'s `check_path/1` for **one** directory: the spelling that really
    /// lands on the code path, or `None` for the `{error, bad_directory}` answer.
    ///
    /// Under `strict` (the default, `init:code_path_choice/0`) the directory is accepted as
    /// written and nothing else. Under `relaxed` the oracle additionally rewrites a
    /// `<…>/<App>/ebin`-shaped **miss** into the archive spellings it probes through
    /// `erl_prim_loader` (`code_server.erl:683-722`), so a directory reachable *inside* a
    /// `<App>.ez` archive is accepted there.
    ///
    /// This runtime has no archive reader — its `prim_file:*` floor is a `std::fs`
    /// implementation, and it carries no `erl_prim_loader` archive support — so no rewritten
    /// candidate could name a directory it would ever load a module from, and a candidate that
    /// is not itself a real directory would put an unsearchable entry on the path. That is why
    /// the relaxed branch adds nothing here; the oracle agrees for every layout this runtime
    /// can serve (a **non-archive** `<App>.ez` directory makes the oracle's own
    /// `read_file_info` fail — `File operation error: eisdir`, verified against OTP 29.1.1 /
    /// erts-17.1 — so `-code_path_choice relaxed` answers `{error, bad_directory}` for a
    /// `…/x/ebin` miss unless `x.ez` really is an archive). The mode is still **read**, so an
    /// unmodeled `-code_path_choice relaxed` on the command line is acknowledged rather than
    /// silently assumed to be `strict`.
    fn check_path(&self, dir: &str) -> Option<String> {
        if Path::new(dir).is_dir() {
            return Some(dir.to_string());
        }
        match self.path_choice_mode() {
            PathChoice::Strict => None,
            PathChoice::Relaxed => None,
        }
    }

    /// The `-code_path_choice` value, as `init:code_path_choice/0` reports it.
    ///
    /// The oracle's rule (`init.erl:1875-1884`, mirrored by the floor's own
    /// [`init:code_path_choice/0`](RuntimeContext::code_path_choice)) is `relaxed` for the
    /// single value `"relaxed"` and `strict` for everything else.
    fn path_choice_mode(&self) -> PathChoice {
        let relaxed = self
            .runtime
            .init_args()
            .get("code_path_choice")
            .is_some_and(|occurrences| {
                matches!(
                    occurrences.as_slice(),
                    [values] if values.len() == 1 && values[0] == "relaxed"
                )
            });
        if relaxed {
            PathChoice::Relaxed
        } else {
            PathChoice::Strict
        }
    }
}

/// `init:code_path_choice/0`'s two values, as [`RuntimeContext::check_path`] consumes them.
///
/// A private spelling of the atom the floor's `init:code_path_choice/0` answers, so the
/// decision is made once, on the parsed command line, rather than by comparing rendered atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathChoice {
    /// `strict` — the default; only a directory as explicitly stated is accepted.
    Strict,
    /// `relaxed` — the oracle also rewrites a miss into its `<App>.ez` archive spellings.
    Relaxed,
}

/// `code_server.erl`'s `get_name/1` (`:643-652`): the name a code-path directory is known by.
///
/// `filename:split/1`, then — for a two-component tail `[DirName, "ebin"]` or a one-component
/// tail `[DirName]` — `DirName` truncated at its first `-` (`discard_after_hyphen/1`), which
/// is how `<…>/stdlib-6.0.2/ebin` is known as `stdlib`. Longer tails drop one component at a
/// time, so `get_name/1` examines the application-shaped tail of any depth.
fn directory_name(dir: &str) -> String {
    let components = split_components(dir);
    let mut tail: &[String] = &components;
    loop {
        match tail {
            [prefix, last] if last == "ebin" => return discard_after_hyphen(prefix),
            [only] => return discard_after_hyphen(only),
            [_, rest @ ..] => tail = rest,
            [] => return String::new(),
        }
    }
}

/// `discard_after_hyphen/1` (`code_server.erl:654-661`): `BaseName` up to, but excluding, its
/// first `-`.
fn discard_after_hyphen(base_name: &str) -> String {
    match base_name.split_once('-') {
        Some((prefix, _)) => prefix.to_string(),
        None => base_name.to_string(),
    }
}

/// `filename:split/1` on the `unix` path type: the leading `/` as its own component, then one
/// component per remaining segment. Oracle-verified: `/a/b` ⇒ `["/", "a", "b"]`,
/// `a/b` ⇒ `["a", "b"]`, `/` ⇒ `["/"]`, `""` ⇒ `[]`.
fn split_components(dir: &str) -> Vec<String> {
    use std::path::Component;
    Path::new(dir)
        .components()
        .filter_map(|component| match component {
            Component::RootDir => Some("/".to_string()),
            Component::CurDir => Some(".".to_string()),
            Component::ParentDir => Some("..".to_string()),
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::Prefix(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! Unit tests for the path-mutating `code:*` entries, driven through the runtime's own
    //! dispatch ([`ExecutionContext::call_bif`]) like every other floor suite.
    //!
    //! [`ExecutionContext::call_bif`]: beam_engine::ExecutionContext::call_bif

    use super::*;
    use beam_engine::terms;
    use crate::native::test_support::{atom, call, chars, runtime, string_of};
    use beam_term::make_small;

    /// A private temporary directory, removed when the test ends.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> TempDir {
            static NEXT: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "beam_rt_code_path_{}_{unique}_{label}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("a private temporary directory");
            TempDir(path)
        }

        fn path(&self) -> String {
            self.0.to_str().expect("an ASCII temporary path").to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The code path as rendered strings.
    fn code_path(runtime: &mut crate::Runtime) -> Vec<String> {
        let path = call(runtime, "code", "get_path", &[]).expect("code:get_path/0");
        terms::list_elements(path)
            .expect("a list")
            .iter()
            .map(|entry| string_of(*entry))
            .collect()
    }

    /// `call(runtime, "code", function, [<dir as a charlist>])` — the argument is materialised
    /// **before** the call so the two do not overlap a mutable borrow of the runtime.
    fn code_call_dir(
        runtime: &mut crate::Runtime,
        function: &str,
        dir: &str,
    ) -> Result<Eterm> {
        let argument = chars(runtime, dir);
        call(runtime, "code", function, &[argument])
    }

    // -- the normaliser -------------------------------------------------------

    /// [`normalize_dir`] against the oracle's `filename:join/1` for one component — every row
    /// of this table was read out of the system `erl` (OTP 29.1.1 / erts-17.1) as
    /// `[filename:join([X]) || X <- …]`.
    #[test]
    fn normalize_dir_reproduces_the_oracles_join_of_one_component() {
        for (input, expected) in [
            ("/x/", "/x"),
            ("//", "/"),
            ("//x//", "/x"),
            ("/a/b/", "/a/b"),
            ("/a//b", "/a/b"),
            ("a/b/", "a/b"),
            ("./x", "./x"),
            (".//x", "./x"),
            ("..", ".."),
            ("a/..", "a/.."),
            ("/", "/"),
            ("/.", "/."),
            ("/./x", "/x"),
            ("x/", "x"),
            ("foo", "foo"),
            ("/tmp/x/../y", "/tmp/x/../y"),
            ("./", "."),
            ("a//", "a"),
            ("/a/./b/", "/a/b"),
            (".", "."),
            ("", ""),
        ] {
            assert_eq!(normalize_dir(input), expected, "filename:join([{input:?}])");
        }
    }

    // -- get_name/1 -----------------------------------------------------------

    /// [`directory_name`] against the oracle's `code_server:get_name/1` shapes: the
    /// application name of an installed library entry, truncated at its first `-`.
    #[test]
    fn directory_name_names_an_application_entry_like_the_oracle() {
        for (input, expected) in [
            ("/usr/lib/erlang/lib/stdlib-6.0.2/ebin", "stdlib"),
            ("/usr/lib/erlang/lib/kernel/ebin", "kernel"),
            ("/tmp/xyz/a9fix_repro", "a9fix_repro"),
            ("a9fix_repro", "a9fix_repro"),
            ("/tmp/x-1.0/y", "y"),
            ("", ""),
        ] {
            assert_eq!(directory_name(input), expected, "get_name({input:?})");
        }
    }

    // -- add_patha / add_pathz / add_path -------------------------------------

    /// `add_patha` **prepends** and **moves**: the head is the added directory, a second call
    /// leaves exactly one entry for it, and `code:get_path/0` is the store the loaders read.
    #[test]
    fn add_patha_moves_a_directory_to_the_head() {
        let dir = TempDir::new("add_patha");
        let mut runtime = runtime();
        let before = code_path(&mut runtime);

        let added =
            code_call_dir(&mut runtime, "add_patha", &dir.path()).expect("code:add_patha/1");
        assert_eq!(runtime.atom_name_of(added), Some("true"));

        let after = code_path(&mut runtime);
        assert_eq!(after[0], dir.path(), "the added directory heads the path");
        assert_eq!(after.len(), before.len() + 1);

        // A second call moves the one entry back to the head rather than duplicating it.
        code_call_dir(&mut runtime, "add_patha", &dir.path())
            .expect("code:add_patha/1 again");
        let again = code_path(&mut runtime);
        assert_eq!(again.len(), after.len(), "no duplicate entry");
        assert_eq!(again[0], dir.path());
        assert_eq!(
            again.iter().filter(|entry| **entry == dir.path()).count(),
            1,
            "exactly one entry"
        );

        // The directory is reachable by the loader: the store the search reads is the same one.
        assert_eq!(
            runtime.code_server.paths()[0],
            std::path::PathBuf::from(dir.path()),
            "the code server really searches the added directory"
        );
    }

    /// `add_pathz` (and the folding `add_path`) **appends**, and a directory already present
    /// keeps its existing position — the oracle's `do_add(last, …)` keyreplace rule.
    #[test]
    fn add_pathz_appends_and_never_moves_an_existing_entry() {
        let first = TempDir::new("add_pathz_a");
        let second = TempDir::new("add_pathz_b");
        let mut runtime = runtime();
        let before = code_path(&mut runtime);

        for dir in [&first, &second] {
            let added =
                code_call_dir(&mut runtime, "add_pathz", &dir.path()).expect("code:add_pathz/1");
            assert_eq!(runtime.atom_name_of(added), Some("true"));
        }
        let after = code_path(&mut runtime);
        assert_eq!(after.len(), before.len() + 2);
        assert_eq!(
            &after[after.len() - 2..],
            &[first.path(), second.path()],
            "`add_pathz` appends, in call order"
        );

        // `add_path/1` is `add_pathz/1` (`code.erl:1075-1077`).
        let third = TempDir::new("add_pathz_c");
        code_call_dir(&mut runtime, "add_path", &third.path()).expect("code:add_path/1");
        let after = code_path(&mut runtime);
        assert_eq!(after.last(), Some(&third.path()), "`add_path` appends too");

        // A repeat of the FIRST directory leaves its position untouched.
        code_call_dir(&mut runtime, "add_pathz", &first.path())
            .expect("code:add_pathz/1 repeat");
        let repeated = code_path(&mut runtime);
        assert_eq!(repeated, after, "an existing entry keeps its position");
        assert_eq!(repeated.iter().filter(|e| **e == first.path()).count(), 1);
    }

    /// The `Cache` argument is accepted for both values (`?is_cache/1`) and changes nothing
    /// observable — the oracle's code-directory cache is not part of `code:get_path/0`.
    #[test]
    fn add_patha_accepts_both_cache_values() {
        let dir = TempDir::new("cache");
        for cache in ["cache", "nocache"] {
            let mut runtime = runtime();
            let cache = atom(&mut runtime, cache);
            let argument = chars(&mut runtime, &dir.path());
            let added = call(&mut runtime, "code", "add_patha", &[argument, cache])
                .expect("code:add_patha/2");
            assert_eq!(runtime.atom_name_of(added), Some("true"));
            assert_eq!(code_path(&mut runtime)[0], dir.path());
        }
    }

    /// A directory that does not exist (or is a regular file) is the oracle's
    /// `{error, bad_directory}`, and the path is left **unchanged**.
    #[test]
    fn add_patha_reports_bad_directory_for_a_missing_or_file_path() {
        let dir = TempDir::new("bad_directory");
        let missing = format!("{}/no_such_dir_xyz", dir.path());
        // A regular file beside it: `check_path/1` is a directory test, not an existence test.
        let regular = dir.0.join("regular.txt");
        std::fs::write(&regular, b"x").expect("a temporary file");
        let regular = regular.to_str().expect("an ASCII path").to_string();

        let mut runtime = runtime();
        let before = code_path(&mut runtime);
        for candidate in [&missing, &regular, &String::new()] {
            let answer =
                code_call_dir(&mut runtime, "add_patha", candidate).expect("code:add_patha/1");
            assert_eq!(
                runtime.format_term(answer, None).expect("printable").trim(),
                "{error,bad_directory}",
                "code:add_patha({candidate:?})"
            );
        }
        assert_eq!(code_path(&mut runtime), before, "the path is unchanged");
    }

    /// The `code.erl` guard: a non-list directory, or a `Cache` that is neither `cache` nor
    /// `nocache`, raises the bare-atom `function_clause` — for all three entries.
    #[test]
    fn the_guard_raises_function_clause_for_bad_arguments() {
        let dir = TempDir::new("guard");
        let mut runtime = runtime();
        let dir_term = chars(&mut runtime, &dir.path());
        let foo = atom(&mut runtime, "foo");
        let bogus = atom(&mut runtime, "bogus");

        for function in ["add_patha", "add_pathz", "add_path"] {
            for args in [
                vec![foo],
                vec![make_small(42)],
                vec![dir_term, bogus],
            ] {
                let error = call(&mut runtime, "code", function, &args)
                    .expect_err("`{function}` must raise for {args:?}");
                assert_eq!(
                    error.as_atom(),
                    Some("function_clause"),
                    "code:{function}(…): {error}"
                );
            }
        }
        // Nothing landed on the path.
        assert!(code_path(&mut runtime).iter().all(|entry| entry != &dir.path()));
    }

    /// `del_path/1` removes by the directory itself, answers `false` for a second (or never
    /// added) one, and matches an entry by its `get_name/1` application name.
    #[test]
    fn del_path_removes_by_directory_and_by_application_name() {
        let dir = TempDir::new("del_path");
        let mut runtime = runtime();
        let dir_term = chars(&mut runtime, &dir.path());
        call(&mut runtime, "code", "add_patha", &[dir_term]).expect("add_patha/1");
        let added = code_path(&mut runtime);
        assert_eq!(added[0], dir.path());

        let removed = call(&mut runtime, "code", "del_path", &[dir_term]).expect("del_path/1");
        assert_eq!(runtime.atom_name_of(removed), Some("true"));
        assert!(
            code_path(&mut runtime).iter().all(|entry| entry != &dir.path()),
            "the directory is gone"
        );

        // Again — and a name that was never on the path.
        for argument in [dir_term, atom(&mut runtime, "never_added_xyz")] {
            let answer = call(&mut runtime, "code", "del_path", &[argument])
                .expect("code:del_path/1");
            assert_eq!(runtime.atom_name_of(answer), Some("false"));
        }

        // The backwards-compatible name form: an `<…>/<app>-<vsn>/ebin` entry is known by
        // `<app>`, so `del_path(<app>)` (an atom) removes it.
        let lib = TempDir::new("del_path_named");
        let ebin = lib.0.join("a9delapp-1.0").join("ebin");
        std::fs::create_dir_all(&ebin).expect("an application-shaped directory");
        let ebin_text = ebin.to_str().expect("an ASCII path").to_string();
        code_call_dir(&mut runtime, "add_patha", &ebin_text).expect("add_patha/1");
        assert_eq!(code_path(&mut runtime)[0], ebin_text);
        let app_name = atom(&mut runtime, "a9delapp");
        let by_name =
            call(&mut runtime, "code", "del_path", &[app_name]).expect("code:del_path/1");
        assert_eq!(
            runtime.atom_name_of(by_name),
            Some("true"),
            "the application name names the directory"
        );
        assert!(code_path(&mut runtime).iter().all(|entry| entry != &ebin_text));
    }

    /// `del_path/1`'s guard (`code.erl:1209`): a non-list/non-atom argument raises
    /// `function_clause`; a list naming no directory is the plain `false`.
    #[test]
    fn del_path_guards_on_atom_or_list() {
        let mut runtime = runtime();
        let error = call(&mut runtime, "code", "del_path", &[make_small(42)])
            .expect_err("code:del_path(42) must raise");
        assert_eq!(error.as_atom(), Some("function_clause"), "{error}");

        let answer = code_call_dir(&mut runtime, "del_path", "no_such_dir_xyz")
            .expect("code:del_path/1");
        assert_eq!(runtime.atom_name_of(answer), Some("false"));
    }

    // -- the code_path_choice reader ------------------------------------------

    /// The `-code_path_choice` reader: `relaxed` only for the single value `"relaxed"`, and
    /// `strict` for every other command line — the rule `init:code_path_choice/0` reports.
    #[test]
    fn code_path_choice_reads_the_first_argument() {
        let relaxed = || {
            let mut config = crate::config::RuntimeConfig::default();
            config
                .init_args
                .record("code_path_choice", vec!["relaxed".to_string()]);
            let mut runtime = crate::Runtime::new(config).expect("a runtime with the flag");
            let mut sink = beam_bifs::NullSink;
            let context =
                crate::context::RuntimeContext::with_output(&mut runtime, &mut sink);
            context.path_choice_mode()
        };
        assert_eq!(relaxed(), PathChoice::Relaxed);

        for values in [vec!["strict"], vec!["foo"], vec![]] {
            let mut config = crate::config::RuntimeConfig::default();
            config.init_args.record(
                "code_path_choice",
                values.iter().map(|value| (*value).to_string()).collect(),
            );
            let mut runtime = crate::Runtime::new(config).expect("a runtime");
            let mut sink = beam_bifs::NullSink;
            let context =
                crate::context::RuntimeContext::with_output(&mut runtime, &mut sink);
            assert_eq!(context.path_choice_mode(), PathChoice::Strict, "{values:?}");
        }
    }

    /// Neither mode accepts a directory that does not exist: `strict` is the default and the
    /// relaxed archive rewrite has no readable candidate here (see
    /// [`RuntimeContext::check_path`]).
    #[test]
    fn both_path_choice_modes_reject_a_missing_directory() {
        let dir = TempDir::new("choice_modes");
        let missing = format!("{}/no_such_dir_xyz/ebin", dir.path());
        for values in [vec![], vec!["relaxed"]] {
            let mut config = crate::config::RuntimeConfig::default();
            if !values.is_empty() {
                config.init_args.record(
                    "code_path_choice",
                    values.iter().map(|value| (*value).to_string()).collect(),
                );
            }
            let mut runtime = crate::Runtime::new(config).expect("a runtime");
            let answer = code_call_dir(&mut runtime, "add_patha", &missing)
                .expect("code:add_patha/1");
            assert_eq!(
                runtime.format_term(answer, None).expect("printable").trim(),
                "{error,bad_directory}",
                "code_path_choice = {values:?}"
            );
        }
    }
}
