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

//! The `code:*` runtime-native floor: the code path, the loaded and preloaded module set,
//! the code server's `interactive`/`embedded` mode and the installation directories.
//!
//! `code` is the module library's front end; its `.beam` (and the `code_server` process
//! behind it) needs facilities outside Milestone-1, so the runtime answers these calls
//! directly from the state of its own [`CodeServer`](crate::CodeServer) and the
//! preloaded-module registry instead of loading a module it cannot execute (see the
//! parent module's docs).
//!
//! `src/native.rs`'s `code:*` behaviour is load-bearing for the differential corpus's
//! `boot/*` cases, all of which currently agree with the oracle.

use std::path::Path;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::Eterm;

use crate::context::RuntimeContext;
use crate::error;

/// The application directories the oracle marks **sticky** (see `code.erl`'s
/// `maybe_stick_dirs/1`: `kernel`, `stdlib` and `compiler`).
///
/// The preloaded set is sticky from boot as well (see [the docs on
/// `is_sticky`](RuntimeContext::is_sticky)), which is how the emulator's own modules
/// (`init`, `erl_prim_loader`, …) — resolved from the `erts` `ebin`, not one of these
/// directories — are sticky too. This list is the *directory* half of the rule only.
const STICKY_APPS: &[&str] = &["kernel", "stdlib", "compiler"];

impl RuntimeContext<'_> {
    /// `code:get_path/0`: the code path, one directory string per entry, in search
    /// order.
    ///
    /// The system library directories (`stdlib`, `kernel`, `erts`) are resolved on first
    /// use, exactly as a module lookup does, so the list is the order
    /// [`CodeServer::find_file`](crate::CodeServer::find_file) searches. Resolution is
    /// deliberately **best-effort** here, matching [`Runtime::find_module_file`]: a
    /// failure leaves the configured `-pa`/`-pz` directories and is reported loudly by
    /// the module *load* that actually needed a system module, never silently by this
    /// listing.
    ///
    /// [`Runtime::find_module_file`]: crate::Runtime::find_module_file
    pub(super) fn code_get_path(&mut self) -> Result<Eterm> {
        let _ = self.runtime.code_server.add_system_paths();
        let dirs: Vec<String> = self
            .runtime
            .code_server
            .paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        self.string_list(&dirs)
    }

    /// `code:all_loaded/0`: `{Module, Beam}` for every preloaded and loaded module.
    ///
    /// `Beam` is the atom `preloaded` for a preloaded module, its `.beam` file path (a
    /// string) for a loaded one, and the **empty string** for one loaded from memory (no
    /// source file: [`CodeServer::load_bytes`]) — see [`Self::beam_location`], which is the
    /// single source of truth for the shape (so `all_loaded/0`, `which/1` and `is_loaded/1`
    /// can never disagree). The preloaded names come first — in
    /// [`erlang:pre_loaded/0`](crate::PreloadedModules::names)'s order — so the list is
    /// never empty, as the oracle's always is. The oracle orders the list by load time;
    /// here the loaded modules that are not preloaded are sorted by name. Neither order is
    /// part of any contract, but the preloaded prefix is the oracle's.
    ///
    /// [`CodeServer::load_bytes`]: crate::CodeServer::load_bytes
    pub(super) fn code_all_loaded(&mut self) -> Result<Eterm> {
        // The preloaded names first, then every loaded module that is not preloaded.
        let mut names: Vec<String> = self
            .runtime
            .preloaded()
            .names()
            .map(str::to_string)
            .collect();
        for name in self.runtime.code_server.loaded() {
            if !self.runtime.preloaded().is_preloaded(&name) {
                names.push(name);
            }
        }

        let mut tuples = Vec::with_capacity(names.len());
        for name in &names {
            let name_atom = self.atom(name)?;
            // A module with no `Beam` at all cannot occur here: every name is either
            // preloaded or loaded, and `beam_location` answers both of those.
            let beam = match self.beam_location(name)? {
                Some(beam) => beam,
                None => self.charlist("")?,
            };
            tuples.push(terms::make_tuple(self, &[name_atom, beam])?);
        }
        terms::make_list(self, &tuples)
    }

    /// `code:which(Module)`: the atom `preloaded` for a preloaded module, the module's
    /// `.beam` path (a string) for one that is loaded or on the code path, and the atom
    /// `non_existing` otherwise.
    pub(super) fn code_which(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        match self.beam_location(&name)? {
            Some(beam) => Ok(beam),
            None => self.atom("non_existing"),
        }
    }

    /// `code:is_loaded(Module)`: `{file, Beam}` for a module that has a `.beam` file (or
    /// is preloaded), and the atom `false` for one that is nowhere to be found.
    ///
    /// Oracle-verified (OTP 29/erts-17.1): `code:is_loaded(lists)` ⇒
    /// `{file,"…/stdlib-8.1/ebin/lists.beam"}`, `code:is_loaded(erlang)` ⇒
    /// `{file,preloaded}` (the *atom*, from the preloaded registry) and
    /// `code:is_loaded(no_such_mod_xyz)` ⇒ `false`. The location is exactly
    /// [`Self::code_which`]'s minus its `non_existing`, so the two can never disagree.
    ///
    /// A non-atom argument cannot name a module and is `badarg` — the local convention
    /// every `code:*` entry already follows (the oracle's own reason is
    /// `function_clause`).
    pub(super) fn code_is_loaded(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        match self.beam_location(&name)? {
            Some(beam) => {
                let file = self.atom("file")?;
                terms::make_tuple(self, &[file, beam])
            }
            None => self.atom("false"),
        }
    }

    /// `code:is_sticky(Module)`: whether `Module` is a **sticky** module.
    ///
    /// A sticky module cannot be (re)loaded by unprivileged code and, in the oracle,
    /// covers two sets: the **preloaded** modules (sticky from boot) and every module
    /// whose `.beam` lives in a `kernel`, `stdlib` or `compiler` application directory
    /// (`code.erl`'s `maybe_stick_dirs/1`). Any other module — including one merely
    /// findable on the code path — answers `false`, which is what the oracle answers for
    /// e.g. `mnesia`.
    ///
    /// A non-atom argument is `badarg` (see [`Self::code_is_loaded`]).
    ///
    /// Oracle-verified (OTP 29/erts-17.1): `lists`, `kernel`, `code`, `proplists`,
    /// `ordsets`, `gen_server`, `erl_eval`, `zlib`, `persistent_term`, `erl_init` and
    /// `erl_prim_loader` are `true`; `mnesia` and `no_such_mod_xyz` are `false`.
    pub(super) fn code_is_sticky_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        let sticky = self.is_sticky(&name);
        self.atom(if sticky { "true" } else { "false" })
    }

    /// `code:is_sticky/2` — a function this OTP release does **not** have.
    ///
    /// Oracle-verified (OTP 29/erts-17.1): `catch code:is_sticky(kernel,stdlib)` ⇒
    /// `{'EXIT',{undef,…}}`. The runtime reproduces that raise exactly (reason atom
    /// `undef`) instead of inventing a value or reporting a capability gap — the function
    /// is genuinely absent from the release, not unimplemented here.
    pub(super) fn code_is_sticky_2(&mut self) -> Result<Eterm> {
        Err(error::undef())
    }

    /// `code:ensure_loaded(Module)`: `{module, Module}` once the module is loaded (or is
    /// preloaded), and `{error, nofile}` when it cannot be found.
    ///
    /// The module is loaded on demand from the code path exactly as a call would resolve
    /// it (the system library directories are seeded first), so a successful answer
    /// really has the module in the shared code area. Oracle-verified (OTP 29/erts-17.1):
    /// `code:ensure_loaded(lists)` ⇒ `{module,lists}`, `code:ensure_loaded(no_such_mod_xyz)`
    /// ⇒ `{error,nofile}`.
    ///
    /// A `.beam` that *is* found but fails to load is **not** reported as `nofile`: the
    /// loader's own error is propagated rather than hidden behind the not-found shape.
    pub(super) fn code_ensure_loaded(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        if self.runtime.preloaded().is_preloaded(&name)
            || self.runtime.code_server.is_loaded(&name)
        {
            return self.module_pair(&name);
        }
        if self.module_file(&name).is_none() {
            return self.no_file();
        }
        self.runtime.code_server.load(&name)?;
        self.module_pair(&name)
    }

    /// The `{module, Module}` pair [`Self::code_ensure_loaded`] answers with.
    fn module_pair(&mut self, name: &str) -> Result<Eterm> {
        let module = self.atom("module")?;
        let name = self.atom(name)?;
        terms::make_tuple(self, &[module, name])
    }

    /// The `{error, nofile}` pair [`Self::code_ensure_loaded`] answers with.
    fn no_file(&mut self) -> Result<Eterm> {
        let error = self.atom("error")?;
        let nofile = self.atom("nofile")?;
        terms::make_tuple(self, &[error, nofile])
    }

    /// Whether `name` is sticky: preloaded, or resolved to a `.beam` under one of the
    /// [`STICKY_APPS`] application directories (see [`Self::code_is_sticky_1`]).
    fn is_sticky(&mut self, name: &str) -> bool {
        if self.runtime.preloaded().is_preloaded(name) {
            return true;
        }
        let Some(path) = self.module_file(name) else {
            return false;
        };
        let path = Path::new(&path);
        STICKY_APPS.iter().any(|app| {
            self.runtime
                .code_server
                .app_dir(app)
                .is_some_and(|dir| path.starts_with(dir))
        })
    }

    /// The `Beam` half of a `{Module, Beam}` / `{file, Beam}` pair: the atom `preloaded`
    /// for a preloaded module, its recorded `.beam` path (a string) for one that is loaded
    /// or on the code path, the **empty string** for one loaded from memory, and `None` when
    /// the module is nowhere to be found.
    ///
    /// The empty string is the oracle's own value, not an invention: `code_server` stores
    /// the filename a module was loaded with, and a module loaded from bytes (the runtime's
    /// [`CodeServer::load_bytes`], i.e. OTP's `load_binary/3` with no filename) has none.
    /// Oracle-verified (OTP 29/erts-17.1) for
    /// `code:load_binary(M, "", Bin)`: `code:which(M)` ⇒ `[]`, `code:is_loaded(M)` ⇒
    /// `{file,[]}` and `code:all_loaded/0` ⇒ `[{M,[]}]`. This runtime has no
    /// `load_binary/3` surface, so a memory-loaded module never carries a non-empty
    /// filename — `[]` is the only honest value, and reporting the atom `preloaded` (which
    /// means "a preloaded OTP module") would be wrong.
    ///
    /// [`CodeServer::load_bytes`]: crate::CodeServer::load_bytes
    fn beam_location(&mut self, name: &str) -> Result<Option<Eterm>> {
        if self.runtime.preloaded().is_preloaded(name) {
            return Ok(Some(self.atom("preloaded")?));
        }
        // Loaded, but with no recorded path: a module loaded from memory (checked before the
        // code-path search, because the loaded filename is what the oracle reports even when
        // a `.beam` of the same name is *also* findable on the path).
        if self.runtime.code_server.loaded_path(name).is_none()
            && self.runtime.code_server.is_loaded(name)
        {
            return Ok(Some(self.charlist("")?));
        }
        match self.module_file(name) {
            Some(path) => Ok(Some(self.charlist(&path)?)),
            None => Ok(None),
        }
    }

    /// `code:lib_dir/0`: the system library root (`<lib>`) the code path's `ebin`
    /// directories live under, as a string.
    ///
    /// # Errors
    ///
    /// A diagnostic when the system code path cannot be resolved (a hard error: without
    /// it there is no `<lib>` to report) or holds no `<lib>/<app>-<vsn>/ebin` entry.
    pub(super) fn code_lib_dir_0(&mut self) -> Result<Eterm> {
        self.runtime.code_server.add_system_paths()?;
        let root = self.runtime.code_server.lib_root().ok_or_else(|| {
            error::message(
                "code:lib_dir/0: the code path has no <lib>/<app>-<vsn>/ebin directory",
            )
        })?;
        self.charlist(&root.display().to_string())
    }

    /// `code:lib_dir(App)`: the application directory `<lib>/<app>-<vsn>` of `App` as a
    /// string, or `{error, bad_name}` when `App` is not an installed application.
    pub(super) fn code_lib_dir_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        let found = match self.atom_name(argument) {
            Some(app) => {
                self.runtime.code_server.add_system_paths()?;
                self.runtime.code_server.app_dir(&app)
            }
            // A non-atom argument names no application.
            None => None,
        };
        match found {
            Some(dir) => self.charlist(&dir.display().to_string()),
            None => {
                let tag = self.atom("error")?;
                let reason = self.atom("bad_name")?;
                terms::make_tuple(self, &[tag, reason])
            }
        }
    }

    /// `code:root_dir/0`: the Erlang installation root the system booted from, as a
    /// string.
    ///
    /// It is the **parent** of the `<lib>` directory [`Self::code_lib_dir_0`] reports —
    /// verified against OTP 29/erts-17.1, where `code:root_dir()` is
    /// `…/lib/erlang` and `code:lib_dir()` is `…/lib/erlang/lib`. The derivation is
    /// therefore `lib_root().parent()`, so both answers come from the one resolved code
    /// path (no second source of truth).
    ///
    /// # Errors
    ///
    /// The same hard errors as [`Self::code_lib_dir_0`] — an unresolvable system code
    /// path, or a `<lib>` entry with no parent component.
    pub(super) fn code_root_dir(&mut self) -> Result<Eterm> {
        self.runtime.code_server.add_system_paths()?;
        let lib_root = self.runtime.code_server.lib_root().ok_or_else(|| {
            error::message(
                "code:root_dir/0: the code path has no <lib>/<app>-<vsn>/ebin directory",
            )
        })?;
        let root = lib_root.parent().ok_or_else(|| {
            error::message("code:root_dir/0: the <lib> directory has no parent component")
        })?;
        self.charlist(&root.display().to_string())
    }

    /// `code:get_mode/0`: the atom `interactive`, or `embedded` for a `-mode embedded`
    /// command line.
    ///
    /// The oracle answers from the code server's start mode, which `code.erl`'s
    /// `start_get_mode/0` (`code.erl:1741-1765`) derives from the **`-mode` init
    /// argument**: `embedded` exactly when `init:get_argument(mode)` is
    /// `{ok, [["embedded"]]}` — the *first* occurrence holds the *single* value
    /// `"embedded"` — and `interactive` for every other command line (no `-mode`, a
    /// valueless `-mode`, a different value, or several `-mode` flags, when the first
    /// one still wins). Oracle-verified (OTP 29/erts-17.1): `code:get_mode()` ⇒
    /// `interactive`; `-mode embedded …` ⇒ `embedded`;
    /// `-mode embedded -mode interactive …` ⇒ `embedded` (with a warning);
    /// `-mode foo …` and a valueless `-mode` ⇒ `interactive`.
    ///
    /// This runtime is head-less and resolves its modules from the code path, so
    /// `interactive` is its real mode; the `-mode embedded` override is honoured because
    /// it is what the release's own `code.erl` does with the same init argument. See
    /// [`Self::code_get_path`] for how the mode-less default behaves.
    pub(super) fn code_get_mode(&mut self) -> Result<Eterm> {
        self.atom(if self.wants_embedded_mode() {
            "embedded"
        } else {
            "interactive"
        })
    }

    /// Whether the `-mode` init argument asked for `embedded`: the [`code.erl`]
    /// `start_get_mode/0` rule — the *first* occurrence must be the *single* string
    /// `"embedded"`.
    ///
    /// [`code.erl`]: crate::native::code
    fn wants_embedded_mode(&self) -> bool {
        let Some(first) = self
            .runtime
            .init_args()
            .get("mode")
            .and_then(|occurrences| occurrences.first().copied())
        else {
            return false;
        };
        first.len() == 1 && first[0] == "embedded"
    }

    /// The `.beam` file of `name`: its recorded load path, its code-path entry, or —
    /// after a best-effort resolution of the system directories — its code-path entry
    /// again.
    ///
    /// `None` when the module is nowhere to be found; the resolution failure itself is
    /// not an error here (see [`Self::code_get_path`]).
    fn module_file(&mut self, name: &str) -> Option<String> {
        if let Some(path) = self.runtime.code_server.loaded_path(name) {
            return Some(path.display().to_string());
        }
        if let Some(path) = self.runtime.code_server.find_file(name) {
            return Some(path.display().to_string());
        }
        if self.runtime.code_server.add_system_paths().is_ok() {
            if let Some(path) = self.runtime.code_server.find_file(name) {
                return Some(path.display().to_string());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `code:*` entries: the code path, the loaded/preloaded module
    //! set and the installation directories.

    use super::*;
    use crate::native::test_support::{atom, call, runtime, string_of, system_runtime};
    use beam_term::{is_atom, make_small};

    #[test]
    fn code_get_path_is_a_list_of_strings() {
        let mut runtime = runtime();
        let path = call(&mut runtime, "code", "get_path", &[]).expect("code:get_path/0");
        // Whatever the code path is, it is a (possibly empty) list of char lists.
        for entry in terms::list_elements(path).expect("a list") {
            assert!(
                terms::list_elements(entry).is_ok(),
                "every code-path entry is a string"
            );
        }
    }

    #[test]
    fn code_get_path_includes_the_resolved_system_dirs() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let path = call(&mut runtime, "code", "get_path", &[]).expect("code:get_path/0");
        let entries: Vec<String> = terms::list_elements(path)
            .expect("a list")
            .iter()
            .map(|entry| string_of(*entry))
            .collect();
        let expected: Vec<String> = runtime
            .code_server
            .paths()
            .iter()
            .map(|dir| dir.display().to_string())
            .collect();
        assert_eq!(entries, expected);
        assert!(
            entries.iter().any(|dir| dir.ends_with("ebin")),
            "the resolved system dirs are on the path: {entries:?}"
        );
    }

    #[test]
    fn code_all_loaded_is_tagged_and_never_empty() {
        let mut runtime = runtime();
        let loaded = call(&mut runtime, "code", "all_loaded", &[]).expect("code:all_loaded/0");
        let entries = terms::list_elements(loaded).expect("a list");
        assert!(
            !entries.is_empty(),
            "the preloaded module names make the list non-empty"
        );
        for entry in &entries {
            assert_eq!(terms::tuple_arity(*entry).expect("a {Module, Beam} pair"), 2);
            let name = terms::tuple_element(*entry, 0).expect("the module name");
            assert!(is_atom(name), "every entry is tagged by a module atom");
            let beam = terms::tuple_element(*entry, 1).expect("the beam location");
            assert!(
                is_atom(beam) || terms::list_elements(beam).is_ok(),
                "the beam is the atom `preloaded` or a path string"
            );
        }
        // The preloaded set comes first, in the oracle's `erlang:pre_loaded/0` order
        // (`persistent_term` heads the list).
        let first = entries[0];
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(first, 0).unwrap()),
            Some("persistent_term")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(first, 1).unwrap()),
            Some("preloaded")
        );
        // Every preloaded name is present, and `erlang` is one of them.
        let names: Vec<Option<&str>> = entries
            .iter()
            .map(|entry| {
                runtime.atom_name_of(terms::tuple_element(*entry, 0).expect("the module name"))
            })
            .collect();
        for preloaded in runtime.preloaded().names() {
            assert!(
                names.contains(&Some(preloaded)),
                "{preloaded} is reported: {names:?}"
            );
        }
    }

    #[test]
    fn code_all_loaded_reports_a_loaded_module_with_its_file() {
        let mut runtime = runtime();
        let slice = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        );
        runtime.load_module(slice).expect("the slice fixture loads");
        let loaded = call(&mut runtime, "code", "all_loaded", &[]).expect("code:all_loaded/0");
        let slice_entry = terms::list_elements(loaded)
            .expect("a list")
            .into_iter()
            .find(|entry| {
                runtime.atom_name_of(terms::tuple_element(*entry, 0).unwrap()) == Some("slice")
            })
            .expect("slice is in all_loaded/0");
        assert_eq!(
            string_of(terms::tuple_element(slice_entry, 1).unwrap()),
            slice
        );
    }

    #[test]
    fn code_which_reports_preloaded_loaded_and_missing() {
        let mut runtime = runtime();
        // A preloaded module is `preloaded` (it has no `.beam` file of its own).
        let init = atom(&mut runtime, "init");
        let which = call(&mut runtime, "code", "which", &[init]).expect("code:which/1");
        assert_eq!(runtime.atom_name_of(which), Some("preloaded"));

        // A module nowhere to be found is `non_existing`.
        let missing = atom(&mut runtime, "definitely_not_a_module");
        let which = call(&mut runtime, "code", "which", &[missing]).expect("code:which/1");
        assert_eq!(runtime.atom_name_of(which), Some("non_existing"));

        // A loaded module reports the file it was loaded from.
        let slice = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        );
        runtime.load_module(slice).expect("the slice fixture loads");
        let slice_atom = atom(&mut runtime, "slice");
        let which = call(&mut runtime, "code", "which", &[slice_atom]).expect("code:which/1");
        assert_eq!(string_of(which), slice);

        // A system module is found through the resolved code path (skip loudly without
        // a system Erlang).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let lists = atom(&mut runtime, "lists");
        let which = call(&mut runtime, "code", "which", &[lists]).expect("code:which/1");
        let text = string_of(which);
        assert!(text.ends_with("lists.beam"), "{text}");
    }

    #[test]
    fn code_lib_dir_of_a_non_atom_is_bad_name() {
        let mut runtime = runtime();
        let answer =
            call(&mut runtime, "code", "lib_dir", &[make_small(42)]).expect("code:lib_dir/1");
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("error")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("bad_name")
        );
    }

    #[test]
    fn code_lib_dir_reports_installed_and_unknown_apps() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let kernel = atom(&mut runtime, "kernel");
        let dir = call(&mut runtime, "code", "lib_dir", &[kernel]).expect("code:lib_dir(kernel)");
        let app_dir = string_of(dir);
        assert!(app_dir.contains("kernel-"), "{app_dir}");
        assert!(
            std::path::Path::new(&app_dir).join("ebin").is_dir(),
            "{app_dir} holds an ebin"
        );

        // `code:lib_dir/0` is the `<lib>` root the app directory lives under.
        let root = call(&mut runtime, "code", "lib_dir", &[]).expect("code:lib_dir/0");
        let root = string_of(root);
        assert!(
            app_dir.starts_with(&root),
            "{root} is not a prefix of {app_dir}"
        );

        // An application that is not installed is `{error, bad_name}`.
        let missing = atom(&mut runtime, "definitely_not_an_otp_app");
        let answer = call(&mut runtime, "code", "lib_dir", &[missing]).expect("code:lib_dir/1");
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("error")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("bad_name")
        );
    }

    #[test]
    fn code_root_dir_is_the_parent_of_the_lib_root() {
        // `code:root_dir/0` is a string and is the parent of `code:lib_dir/0`'s `<lib>`
        // directory — this is what the differential `boot/root_dir_is_string` case checks
        // (`is_list(code:root_dir())`).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let root = call(&mut runtime, "code", "root_dir", &[]).expect("code:root_dir/0");
        assert!(beam_term::is_list(root), "root_dir/0 is a string");
        let root = string_of(root);
        let lib = call(&mut runtime, "code", "lib_dir", &[]).expect("code:lib_dir/0");
        let lib = string_of(lib);
        assert_eq!(
            std::path::Path::new(&lib)
                .parent()
                .map(|parent| parent.to_string_lossy().to_string()),
            Some(root.clone()),
            "{root} must be the parent of {lib}"
        );
        assert!(
            std::path::Path::new(&root).is_dir(),
            "{root} is a directory"
        );
    }

    /// The atom `is_loaded`/`is_sticky` answer for the module `name` — a fresh atom in the
    /// shared table, so the argument names the module by name, not by a stale index.
    fn module_atom(runtime: &mut crate::Runtime, name: &str) -> Eterm {
        atom(runtime, name)
    }

    #[test]
    fn code_is_loaded_reports_file_preloaded_and_false() {
        let mut runtime = runtime();
        // A module nowhere to be found is the atom `false`.
        let missing = module_atom(&mut runtime, "definitely_not_a_module");
        let answer = call(&mut runtime, "code", "is_loaded", &[missing]).expect("is_loaded/1");
        assert_eq!(runtime.atom_name_of(answer), Some("false"));

        // A preloaded module answers `{file, preloaded}` — the *atom*, from the registry.
        let init = module_atom(&mut runtime, "init");
        let answer = call(&mut runtime, "code", "is_loaded", &[init]).expect("is_loaded/1");
        assert_eq!(terms::tuple_arity(answer).expect("a pair"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("file")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("preloaded")
        );

        // A loaded module reports the file it was loaded from.
        let slice = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        );
        runtime.load_module(slice).expect("the slice fixture loads");
        let slice_atom = module_atom(&mut runtime, "slice");
        let answer = call(&mut runtime, "code", "is_loaded", &[slice_atom]).expect("is_loaded/1");
        assert_eq!(string_of(terms::tuple_element(answer, 1).unwrap()), slice);

        // A system module is found through the resolved code path and agrees with
        // `code:which/1` (the two share the location helper).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let lists = module_atom(&mut runtime, "lists");
        let loaded = call(&mut runtime, "code", "is_loaded", &[lists]).expect("is_loaded/1");
        let which = call(&mut runtime, "code", "which", &[lists]).expect("which/1");
        assert_eq!(
            string_of(terms::tuple_element(loaded, 1).unwrap()),
            string_of(which),
            "is_loaded/1 and which/1 must agree on the location"
        );
        assert!(string_of(which).ends_with("lists.beam"));
    }

    #[test]
    fn code_is_sticky_matches_the_oracle() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        // Every oracle-`true` module this runtime can resolve: the preloaded set plus the
        // `kernel`/`stdlib`-directory modules.
        for name in [
            "lists",
            "kernel",
            "code",
            "proplists",
            "ordsets",
            "gen_server",
            "erl_eval",
            "zlib",
            "persistent_term",
            "erl_init",
            "erl_prim_loader",
        ] {
            let module = module_atom(&mut runtime, name);
            let answer = call(&mut runtime, "code", "is_sticky", &[module]).expect("is_sticky/1");
            assert_eq!(
                runtime.atom_name_of(answer),
                Some("true"),
                "{name} is sticky"
            );
        }
        // A module that is not installed on this code path is honestly not sticky.
        for name in ["mnesia", "definitely_not_a_module"] {
            let module = module_atom(&mut runtime, name);
            let answer = call(&mut runtime, "code", "is_sticky", &[module]).expect("is_sticky/1");
            assert_eq!(
                runtime.atom_name_of(answer),
                Some("false"),
                "{name} is not sticky"
            );
        }
    }

    #[test]
    fn code_is_sticky_2_is_undef_because_the_function_does_not_exist() {
        // Oracle-verified: this OTP release has no `code:is_sticky/2`, so the call raises
        // `error:undef` — never an invented value and never a capability gap.
        let mut runtime = runtime();
        let kernel = module_atom(&mut runtime, "kernel");
        let stdlib = module_atom(&mut runtime, "stdlib");
        let error = call(&mut runtime, "code", "is_sticky", &[kernel, stdlib])
            .expect_err("is_sticky/2 does not exist");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
        assert!(
            crate::error::unsupported_reason(&error).is_none(),
            "undef is a genuine error, not a capability gap: {error}"
        );
    }

    #[test]
    fn code_ensure_loaded_reports_module_or_nofile() {
        let mut runtime = runtime();
        // A missing module is `{error, nofile}`.
        let missing = module_atom(&mut runtime, "definitely_not_a_module");
        let answer = call(&mut runtime, "code", "ensure_loaded", &[missing]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("error")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("nofile")
        );

        // A preloaded module is `{module, M}` without touching the code path.
        let init = module_atom(&mut runtime, "init");
        let answer = call(&mut runtime, "code", "ensure_loaded", &[init]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("module")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("init")
        );

        // A system module is loaded on demand and reported as `{module, M}`.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let lists = module_atom(&mut runtime, "lists");
        let answer = call(&mut runtime, "code", "ensure_loaded", &[lists]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 0).unwrap()),
            Some("module")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(answer, 1).unwrap()),
            Some("lists")
        );
        assert!(runtime.code_server.is_loaded("lists"), "the module really loaded");
        // A second call is answered from the cache.
        let again = call(&mut runtime, "code", "ensure_loaded", &[lists]).expect("ensure/1");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(again, 0).unwrap()),
            Some("module")
        );
    }

    #[test]
    fn code_all_loaded_pins_the_preloaded_atom_and_the_file_string() {
        // The oracle's entry shapes: a preloaded module's `Beam` is the **atom**
        // `preloaded`, a loaded module's is its `.beam` **path string**.
        // `code:all_loaded()` in the oracle: `[X || {M,X} <- code:all_loaded(), M =:= erlang]`
        // ⇒ `[preloaded]`.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        runtime.code_server.load("lists").expect("stdlib lists loads");
        let loaded = call(&mut runtime, "code", "all_loaded", &[]).expect("all_loaded/0");
        let entries = terms::list_elements(loaded).expect("a list");

        let erlang = entries
            .iter()
            .find(|entry| {
                runtime.atom_name_of(terms::tuple_element(**entry, 0).unwrap()) == Some("erlang")
            })
            .expect("erlang is in all_loaded/0");
        let beam = terms::tuple_element(*erlang, 1).unwrap();
        assert!(
            is_atom(beam),
            "a preloaded module's beam is the atom `preloaded`, got {beam:?}"
        );
        assert_eq!(runtime.atom_name_of(beam), Some("preloaded"));

        let lists = entries
            .iter()
            .find(|entry| {
                runtime.atom_name_of(terms::tuple_element(**entry, 0).unwrap()) == Some("lists")
            })
            .expect("lists is in all_loaded/0");
        let beam = terms::tuple_element(*lists, 1).unwrap();
        assert!(
            terms::list_elements(beam).is_ok(),
            "a loaded module's beam is a path string, got {beam:?}"
        );
        assert!(string_of(beam).ends_with("lists.beam"));
    }

    // -- get_mode/0 -----------------------------------------------------------

    /// `code:get_mode/0`, driven through the entry itself: the floor's `try_native`
    /// dispatch arm for it is a registration in `src/native.rs`, outside this node (see
    /// `src/native/CONTEXT.md`), so `call` cannot reach it.
    fn get_mode(runtime: &mut crate::Runtime) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = crate::context::RuntimeContext::with_output(runtime, &mut sink);
        context.code_get_mode().expect("code:get_mode/0")
    }

    #[test]
    fn code_get_mode_is_interactive_by_default() {
        // Oracle-verified (OTP 29/erts-17.1): `code:get_mode()` ⇒ `interactive` — the
        // release's default when no `-mode` was given.
        let mut runtime = runtime();
        let mode = get_mode(&mut runtime);
        assert_eq!(runtime.atom_name_of(mode), Some("interactive"));
    }

    #[test]
    fn code_get_mode_follows_the_first_mode_argument() {
        // The `code.erl` `start_get_mode/0` rule, oracle-verified (OTP 29/erts-17.1):
        // `-mode embedded` ⇒ `embedded`; `-mode embedded -mode interactive` ⇒ `embedded`
        // (the first occurrence wins); `-mode foo` ⇒ `interactive`; a valueless `-mode`
        // ⇒ `interactive`. The first occurrence must hold the *single* value `embedded`,
        // which is why the two-value form is `interactive` here.
        for (occurrences, expected) in [
            (vec![vec!["embedded"]], "embedded"),
            (vec![vec!["embedded"], vec!["interactive"]], "embedded"),
            (vec![vec!["foo"]], "interactive"),
            (vec![vec!["interactive"]], "interactive"),
            (vec![Vec::<&str>::new()], "interactive"),
            (vec![vec!["embedded", "extra"]], "interactive"),
        ] {
            let mut config = crate::config::RuntimeConfig::default();
            for values in &occurrences {
                config.init_args.record(
                    "mode",
                    values.iter().map(|value| (*value).to_string()).collect(),
                );
            }
            let mut runtime = crate::Runtime::new(config).expect("a runtime with -mode");
            let mode = get_mode(&mut runtime);
            assert_eq!(runtime.atom_name_of(mode), Some(expected), "{occurrences:?}");
        }
    }
}
