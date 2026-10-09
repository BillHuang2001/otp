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

//! The `application:*` runtime-native floor: the runtime's **application configuration
//! view** — the properties of the OTP applications this runtime consists of.
//!
//! This file holds two halves: the production floor — the `application:*` entries plus the
//! `.app`-resource reader ([`parse_resource`]) they are answered from, roughly the first
//! 700 lines — and its colocated `#[cfg(test)] mod tests` below. The file legitimately reads
//! past the workspace's ~1000-line baseline because of that comprehensive colocated test
//! module (every BIF's oracle shape, every resource-parser construct, and the quoted-atom /
//! escape handling); the production half is well within the guideline and is not split
//! further, since the parser and the entries that use it belong together.
//!
//! `application` is an OTP application module whose `.beam` (and the
//! `application_controller`/`application_master` processes behind it) needs facilities
//! outside Milestone-1: a live process/message machinery, a group leader, a `gen_server`
//! loop and a loaded-application registry populated by the boot script. This runtime
//! therefore answers the calls directly from its **own** app-config view — the `.app`
//! resource files of the applications its code path resolves (see [`super`]) — instead of
//! loading a module it cannot execute.
//!
//! # Where the answers come from
//!
//! Every answer is derived from `<lib>/<app>-<vsn>/ebin/<app>.app`, the plain-text
//! *application resource* file `systools`/`app_file` reads, resolved through the code
//! server's own [`app_dir`](crate::CodeServer::app_dir) — the same installation the module
//! loader uses, so there is no second source of truth. The file is the Erlang **extended
//! term** text of the shipping OTP files: `%%` comment lines followed by
//! `{application, Name, [{description, "..."}, {vsn, "..."}, {modules, [...]}, …]}.` —
//! unquoted and quoted atoms, double-quoted strings, integers, lists and tuples, and
//! nothing else (verified against OTP 29/erts-17.1's `kernel-11.0.4` and `stdlib-8.1`).
//! [`parse_resource`] implements exactly that subset and reports anything outside it as a
//! typed error naming the offending construct — never a silently mis-read value.
//!
//! A property is answered **generically** (`get_key/2` must answer any proplist key, not
//! just `description`/`vsn`), so the whole proplist is parsed and its values are
//! materialized through [`RuntimeContext::materialize`]: a name is interned into the one
//! shared atom table (`self.atom`) and a string becomes a char list (`self.charlist`).
//!
//! # Non-atom arguments never raise
//!
//! Oracle-verified (OTP 29/erts-17.1): `application:get_env(42)`,
//! `application:get_env(nope, bogus)`, `application:get_env(kernel, 42)`,
//! `application:get_all_env(42)` and `application:get_key(42, vsn)` answer `undefined` /
//! `[]` — an application or a key that no atom names simply is not there. Every entry here
//! therefore answers its not-found shape for a non-atom argument rather than raising
//! `badarg`.
//!
//! # The one documented deviation
//!
//! `application:get_all_env(kernel)` in the oracle reports the **runtime's** environment,
//! which the kernel application's own start injects extra entries into: it lists **nine**
//! pairs where `kernel.app` declares **seven**. The two extra ones are `logger` (the
//! default handler configuration kernel's start installs) and `connect_all` (set on the
//! distribution/global startup). This runtime runs no application start, so it reports the
//! file's **declared** environment honestly — the seven the file lists, never a fabricated
//! injected entry. The declared entries themselves agree with the oracle key-for-key
//! (`net_ticktime` ⇒ `60`, `logger_level` ⇒ `notice`, …); only their *order* differs,
//! because the oracle's is its application controller's internal storage order, which no
//! contract pins. (`length(application:get_all_env(kernel))` ⇒ oracle 9, here 7.)

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{try_make_small, Eterm};

use crate::context::RuntimeContext;
use crate::error;

/// The OTP applications this Milestone-1 runtime consists of, in the order
/// `application:loaded_applications/0` reports them.
///
/// The runtime's code path resolves exactly `kernel`, `stdlib` and the emulator's own
/// `erts` (the `ebin` directories [`CodeServer`](crate::CodeServer) puts on the path). The
/// oracle never reports `erts` as a *loaded* application — it **is** the emulator, not an
/// application — so it must not be invented here; `code:lib_dir(erts)` still resolves it
/// through the code server, which is a different question. `kernel` and `stdlib` are the
/// two the oracle reports (`erl -noshell -eval 'application:loaded_applications().'` ⇒
/// `[{kernel,"ERTS  CXC 138 10","11.0.4"},{stdlib,"ERTS  CXC 138 10","8.1"}]`, verified
/// against OTP 29/erts-17.1).
const LOADED_APPLICATIONS: &[&str] = &["kernel", "stdlib"];

impl RuntimeContext<'_> {
    // -- application:get_env/1,2,3 --------------------------------------------

    /// `application:get_env(Key)`: always the atom `undefined`.
    ///
    /// The `/1` form is the deprecated "the `Key` of *the calling process's* application"
    /// query: it resolves the application a process belongs to and looks the key up in
    /// *that* application's environment. This runtime runs no application, so no process
    /// belongs to one — there is no environment to look in and the answer is `undefined`
    /// for every key, exactly as the oracle answers for a process that belongs to no
    /// application (verified against OTP 29/erts-17.1).
    pub(super) fn application_get_env_1(&mut self) -> Result<Eterm> {
        self.atom("undefined")
    }

    /// `application:get_env(App, Key)`: `{ok, Value}` when `App` resolves and `Key` is in
    /// its declared environment, `undefined` otherwise.
    ///
    /// `App` and `Key` must both be atoms; anything else names no application/key and is
    /// the not-found shape (see the module docs).
    pub(super) fn application_get_env_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let app = args.first().copied().and_then(|arg| self.atom_name(arg));
        let key = args.get(1).copied().and_then(|arg| self.atom_name(arg));
        let (Some(app), Some(key)) = (app, key) else {
            return self.atom("undefined");
        };
        let Some(pairs) = self.app_env_props(&app)? else {
            return self.atom("undefined");
        };
        match prop_value(&pairs, &key) {
            Some(value) => {
                let value = self.materialize(value)?;
                let ok = self.atom("ok")?;
                terms::make_tuple(self, &[ok, value])
            }
            None => self.atom("undefined"),
        }
    }

    /// `application:get_env(App, Key, Default)`: the declared value of `Key`, or `Default`
    /// when the application, the key or the environment is missing.
    ///
    /// Unlike the `/2` form the answer is the value **itself**, not `{ok, Value}`.
    /// Oracle-verified: `application:get_env(kernel, net_ticktime, def)` ⇒ `60`,
    /// `application:get_env(kernel, nope, def)` ⇒ `def`, `application:get_env(nope, bogus,
    /// def)` ⇒ `def`, `application:get_env(kernel, 42, def)` ⇒ `def`.
    pub(super) fn application_get_env_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let default = *args.get(2).ok_or(BeamError::BadArg)?;
        let app = args.first().copied().and_then(|arg| self.atom_name(arg));
        let key = args.get(1).copied().and_then(|arg| self.atom_name(arg));
        let (Some(app), Some(key)) = (app, key) else {
            return Ok(default);
        };
        let Some(pairs) = self.app_env_props(&app)? else {
            return Ok(default);
        };
        match prop_value(&pairs, &key) {
            Some(value) => self.materialize(value),
            None => Ok(default),
        }
    }

    /// `application:get_all_env(App)`: the application's `env` property, a list of
    /// `{Key, Value}` pairs, or `[]`.
    ///
    /// `[]` is the answer for an application that declares no environment (stdlib does
    /// not — oracle: `application:get_all_env(stdlib)` ⇒ `[]`), for an application that
    /// does not resolve, and for a non-atom argument. See the module docs for the one
    /// honest deviation: this is the **declared** environment, not the one the oracle's
    /// running application has injected entries into.
    pub(super) fn application_get_all_env_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pairs = match args.first().copied().and_then(|arg| self.atom_name(arg)) {
            Some(app) => self.app_env_props(&app)?.unwrap_or_default(),
            None => Vec::new(),
        };
        let mut entries = Vec::with_capacity(pairs.len());
        for (key, value) in &pairs {
            let key = self.materialize(key)?;
            let value = self.materialize(value)?;
            entries.push(terms::make_tuple(self, &[key, value])?);
        }
        terms::make_list(self, &entries)
    }

    /// `application:get_application/0`: always the atom `undefined`.
    ///
    /// `application.erl:1007-1008` is
    /// `get_application() -> application_controller:get_application(group_leader())`: the
    /// `/0` form resolves the application of the **calling process's group leader**. This
    /// runtime runs no application controller (see the module docs) and no application
    /// owns any process, so there is no application to name — exactly as the oracle
    /// answers for a process that belongs to none (verified against OTP 29/erts-17.1:
    /// `application:get_application()` ⇒ `undefined`). The `/1` form is not claimed; it
    /// would have to reason about a pid/module's owning application.
    pub(super) fn application_get_application_0(&mut self) -> Result<Eterm> {
        self.atom("undefined")
    }
    // -- application:loaded_applications/0, which_applications/0 --------------

    /// `application:loaded_applications/0`: `{Application, Description, Vsn}` for every
    /// application of [`LOADED_APPLICATIONS`], in that order.
    ///
    /// An application whose resource file (or whose `description`/`vsn` property) cannot
    /// be resolved is **omitted** — the triple would otherwise have to be fabricated.
    /// Oracle-verified (OTP 29/erts-17.1):
    /// `[{kernel,"ERTS  CXC 138 10","11.0.4"},{stdlib,"ERTS  CXC 138 10","8.1"}]`.
    pub(super) fn application_loaded_applications(&mut self) -> Result<Eterm> {
        self.application_triples(false)
    }

    /// `application:which_applications/0`: the same triples in **reverse** order.
    ///
    /// Oracle-verified (OTP 29/erts-17.1):
    /// `[{stdlib,"ERTS  CXC 138 10","8.1"},{kernel,"ERTS  CXC 138 10","11.0.4"}]` — the
    /// same pair `loaded_applications/0` reports, reversed. The oracle derives the two
    /// lists from different internal sources (the loaded set of the application master
    /// versus the running set of the application controller), so their orders differ; only
    /// membership is meaningful and no contract pins the order, but agreeing with the
    /// oracle costs one `reverse` and is worth it.
    pub(super) fn application_which_applications(&mut self) -> Result<Eterm> {
        self.application_triples(true)
    }

    // -- application:get_key/2 ------------------------------------------------

    /// `application:get_key(App, Key)`: `{ok, Value}` for any property of the
    /// application's resource file, or `undefined`.
    ///
    /// The lookup is **generic** — the whole proplist is parsed and `Key` looked up in it
    /// — so `vsn`, `description`, `modules`, `applications`, `registered`, `mod`,
    /// `runtime_dependencies` and any other key the file declares are all answered the
    /// same way (oracle-verified: `get_key(kernel, vsn)` ⇒ `{ok,"11.0.4"}`,
    /// `get_key(kernel, description)` ⇒ `{ok,"ERTS  CXC 138 10"}`,
    /// `get_key(stdlib, applications)` ⇒ `{ok,[kernel]}`,
    /// `get_key(kernel, modules)` ⇒ `{ok,[<module atoms>]}`,
    /// `get_key(kernel, nope_key)` ⇒ `undefined`, `get_key(bogusapp, vsn)` ⇒ `undefined`,
    /// `get_key(42, vsn)` ⇒ `undefined`).
    pub(super) fn application_get_key_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let app = args.first().copied().and_then(|arg| self.atom_name(arg));
        let key = args.get(1).copied().and_then(|arg| self.atom_name(arg));
        let (Some(app), Some(key)) = (app, key) else {
            return self.atom("undefined");
        };
        let Some(props) = self.app_props(&app)? else {
            return self.atom("undefined");
        };
        match prop_value(&props, &key) {
            Some(value) => {
                let value = self.materialize(value)?;
                let ok = self.atom("ok")?;
                terms::make_tuple(self, &[ok, value])
            }
            None => self.atom("undefined"),
        }
    }

    // -- helpers --------------------------------------------------------------

    /// The `{Application, Description, Vsn}` triples of [`LOADED_APPLICATIONS`], in the
    /// order that list gives (or reversed for
    /// [`application_which_applications`](Self::application_which_applications)).
    ///
    /// The resource file is read and parsed **once per application** and both properties
    /// are taken from that one parse. An application the code path cannot resolve, or
    /// whose file declares no `description`/`vsn`, contributes no triple — the runtime
    /// never invents one.
    fn application_triples(&mut self, reversed: bool) -> Result<Eterm> {
        let mut apps: Vec<&str> = LOADED_APPLICATIONS.to_vec();
        if reversed {
            apps.reverse();
        }
        let mut triples = Vec::with_capacity(apps.len());
        for app in apps {
            let Some(props) = self.app_props(app)? else {
                continue;
            };
            let (Some(description), Some(vsn)) =
                (prop_value(&props, "description"), prop_value(&props, "vsn"))
            else {
                continue;
            };
            let name = self.atom(app)?;
            let description = self.materialize(description)?;
            let vsn = self.materialize(vsn)?;
            triples.push(terms::make_tuple(self, &[name, description, vsn])?);
        }
        terms::make_list(self, &triples)
    }

    /// The `{Key, Value}` proplist of the application resource file of `app`, or `None`
    /// when the application or its `.app` file cannot be resolved.
    ///
    /// Resolution goes through the runtime's own code server (best-effort
    /// [`add_system_paths`](crate::CodeServer::add_system_paths) first, exactly as a module
    /// lookup does), so the file read is the one the running installation actually uses.
    /// A **missing** `<app>.app` is an application that is not installed, which is the
    /// honest `None`.
    ///
    /// # Errors
    ///
    /// A **typed error** when the file is there but cannot be read, or does not hold the
    /// `{application, Name, Props}` shape this reader implements — never a silent `None`
    /// that would hide a broken installation.
    fn app_props(&mut self, app: &str) -> Result<Option<Vec<(AppTerm, AppTerm)>>> {
        // A failure to resolve the system code path is not fatal here: it is reported
        // loudly by the module *load* that needed it, and an application the runtime
        // cannot find is honestly absent (see `native::code`).
        let _ = self.runtime.code_server.add_system_paths();
        let Some(dir) = self.runtime.code_server.app_dir(app) else {
            return Ok(None);
        };
        let path = dir.join("ebin").join(format!("{app}.app"));
        // A *missing* resource file is an application that cannot be resolved (`None`); a
        // file that is there but cannot be read (an unreadable permission mode, a
        // directory in its place, …) is a broken installation and a **typed error** —
        // never silently reported as "no such application".
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(error::message(format!(
                    "application: cannot read {}: {source}",
                    path.display()
                )))
            }
        };
        let term = parse_resource(&text).map_err(|detail| {
            error::message(format!("application: {}: {detail}", path.display()))
        })?;

        let AppTerm::Tuple(outer) = term else {
            return Err(error::message(format!(
                "application: {}: not an {{application, Name, Properties}} tuple",
                path.display()
            )));
        };
        let props = match <[AppTerm; 3]>::try_from(outer) {
            Ok([tag, _name, props]) => {
                if !matches!(&tag, AppTerm::Atom(name) if name == "application") {
                    return Err(error::message(format!(
                        "application: {}: the tuple is not tagged `application`",
                        path.display()
                    )));
                }
                props
            }
            Err(_) => {
                return Err(error::message(format!(
                    "application: {}: the outer tuple is not a 3-tuple",
                    path.display()
                )))
            }
        };
        let AppTerm::List(items) = props else {
            return Err(error::message(format!(
                "application: {}: the properties are not a list",
                path.display()
            )));
        };

        let mut pairs = Vec::with_capacity(items.len());
        for item in items {
            match item {
                AppTerm::Tuple(pair) if pair.len() == 2 => {
                    pairs.push((pair[0].clone(), pair[1].clone()))
                }
                other => {
                    return Err(error::message(format!(
                        "application: {}: the property {other:?} is not a {{Key, Value}} pair",
                        path.display()
                    )))
                }
            }
        }
        Ok(Some(pairs))
    }

    /// The `{Key, Value}` pairs of `app`'s `env` property, or `None` when the application
    /// cannot be resolved.
    ///
    /// An application resource with no `env` property declares an empty environment, which
    /// is `Some([])` — distinguishable from the unresolvable `None` by the caller even
    /// though both answer `[]`.
    ///
    /// # Errors
    ///
    /// A typed error when the `env` property is present but is not a list of `{Key, Value}`
    /// pairs (a malformed resource file, never silently ignored).
    fn app_env_props(&mut self, app: &str) -> Result<Option<Vec<(AppTerm, AppTerm)>>> {
        let Some(props) = self.app_props(app)? else {
            return Ok(None);
        };
        let Some(AppTerm::List(items)) = prop_value(&props, "env") else {
            return Ok(Some(Vec::new()));
        };
        let mut pairs = Vec::with_capacity(items.len());
        for item in items {
            match item {
                AppTerm::Tuple(pair) if pair.len() == 2 => {
                    pairs.push((pair[0].clone(), pair[1].clone()))
                }
                other => {
                    return Err(error::message(format!(
                        "application {app}: the `env` entry {other:?} is not a {{Key, Value}} pair"
                    )))
                }
            }
        }
        Ok(Some(pairs))
    }

    /// Materializes a parsed application-resource term as a runtime term on the engine's
    /// heap: an atom is interned into the one shared atom table, a string becomes a char
    /// list, a list/tuple is built recursively, and an integer must fit a small int (the
    /// frozen term layout's 60-bit immediate range).
    ///
    /// # Errors
    ///
    /// [`BeamError::SystemLimit`] for an integer outside the small-int range (the `.app`
    /// files of the shipping applications carry none — a larger integer would need a bignum,
    /// which a *term layout* can express but this reader deliberately does not build) and
    /// whatever the atom/term construction reports.
    fn materialize(&mut self, term: &AppTerm) -> Result<Eterm> {
        match term {
            AppTerm::Atom(name) => self.atom(name),
            AppTerm::Int(value) => try_make_small(*value).map_err(|_| BeamError::SystemLimit),
            AppTerm::Str(text) => self.charlist(text),
            AppTerm::List(items) => {
                let mut elems = Vec::with_capacity(items.len());
                for item in items {
                    elems.push(self.materialize(item)?);
                }
                terms::make_list(self, &elems)
            }
            AppTerm::Tuple(items) => {
                let mut elems = Vec::with_capacity(items.len());
                for item in items {
                    elems.push(self.materialize(item)?);
                }
                terms::make_tuple(self, &elems)
            }
        }
    }
}

/// The value of the proplist key `key` in `props` (the first matching pair).
///
/// A key is an atom in every shipping application resource; a key that is not an atom
/// simply never matches, so the caller answers its not-found shape.
fn prop_value<'p>(props: &'p [(AppTerm, AppTerm)], key: &str) -> Option<&'p AppTerm> {
    props
        .iter()
        .find(|(name, _)| matches!(name, AppTerm::Atom(atom) if atom == key))
        .map(|(_, value)| value)
}

// ---------------------------------------------------------------------------
// The application-resource text reader
// ---------------------------------------------------------------------------

/// The parsed form of the Erlang **extended term** text an application resource (`.app`)
/// file is written in.
///
/// Only the shapes the shipping OTP `.app` files use are represented; the reader
/// ([`parse_resource`]) reports anything else rather than approximating it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AppTerm {
    /// An atom, quoted or unquoted (`kernel`, `'hello world'`).
    Atom(String),
    /// An integer (`60`).
    Int(i64),
    /// A double-quoted string (`"ERTS  CXC 138 10"`).
    Str(String),
    /// A list (`[application, code, …]`).
    List(Vec<AppTerm>),
    /// A tuple (`{vsn, "11.0.4"}`).
    Tuple(Vec<AppTerm>),
}

/// Parses the application resource text `source` into a single term.
///
/// The grammar is the subset the shipping OTP `.app` files are written in: `%`-to-
/// end-of-line comments, unquoted and single-quoted atoms, double-quoted strings (with the
/// usual `\`-escapes), integers, lists and tuples, plus the single terminating `.` the
/// files end with. Anything outside it — a float, a binary, a char literal, a map, an
/// improper list tail, a record, unclosed text, text after the term — is a typed error
/// naming the construct and its offset, so a file this reader does not fully understand can
/// never be silently mis-read.
fn parse_resource(source: &str) -> std::result::Result<AppTerm, String> {
    let mut parser = ResourceParser {
        source,
        position: 0,
    };
    let term = parser.term()?;
    parser.skip_trivia();
    // The terminating full stop of an Erlang term (both shipping `.app` files end with
    // `]}.`). It is part of the *file*, not of the term, so it is consumed here.
    if parser.peek() == Some('.') {
        parser.bump();
        parser.skip_trivia();
    }
    if parser.position < parser.source.len() {
        return Err(format!(
            "unexpected trailing text at byte {}",
            parser.position
        ));
    }
    Ok(term)
}

/// The recursive-descent reader behind [`parse_resource`].
struct ResourceParser<'a> {
    /// The whole resource text.
    source: &'a str,
    /// The byte offset of the next unread character.
    position: usize,
}

impl ResourceParser<'_> {
    /// The not-yet-read text.
    fn rest(&self) -> &str {
        &self.source[self.position..]
    }

    /// The next character, without consuming it.
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Consumes and returns the next character.
    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += character.len_utf8();
        Some(character)
    }

    /// Consumes the next character, which must be `expected`.
    fn expect(&mut self, expected: char) -> std::result::Result<(), String> {
        match self.peek() {
            Some(found) if found == expected => {
                self.bump();
                Ok(())
            }
            Some(found) => Err(format!(
                "expected {expected:?} but found {found:?} at byte {}",
                self.position
            )),
            None => Err(format!("expected {expected:?} but the input ended")),
        }
    }

    /// Skips whitespace and `%`-to-end-of-line comments.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(character) if character.is_whitespace() => {
                    self.bump();
                }
                Some('%') => {
                    while self.peek().is_some_and(|character| character != '\n') {
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    /// Reads one term.
    fn term(&mut self) -> std::result::Result<AppTerm, String> {
        self.skip_trivia();
        let Some(character) = self.peek() else {
            return Err("a term was expected but the input ended".to_string());
        };
        match character {
            '{' => self.tuple(),
            '[' => self.list(),
            '"' => Ok(AppTerm::Str(self.quoted('"')?)),
            '\'' => Ok(AppTerm::Atom(self.quoted('\'')?)),
            '0'..='9' => self.integer(),
            '-' if self
                .rest()
                .chars()
                .nth(1)
                .is_some_and(|c| c.is_ascii_digit()) =>
            {
                self.integer()
            }
            'a'..='z' => Ok(AppTerm::Atom(self.unquoted_atom())),
            other => Err(format!(
                "{other:?} at byte {} is outside the supported application-resource subset",
                self.position
            )),
        }
    }

    /// Reads a tuple `{…}`.
    fn tuple(&mut self) -> std::result::Result<AppTerm, String> {
        self.expect('{')?;
        let mut elements = Vec::new();
        self.skip_trivia();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(AppTerm::Tuple(elements));
        }
        loop {
            elements.push(self.term()?);
            self.skip_trivia();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    return Ok(AppTerm::Tuple(elements));
                }
                Some(found) => {
                    return Err(format!(
                        "expected `,` or `}}` but found {found:?} at byte {}",
                        self.position
                    ))
                }
                None => return Err("the tuple is not closed".to_string()),
            }
        }
    }

    /// Reads a list `[…]`.
    fn list(&mut self) -> std::result::Result<AppTerm, String> {
        self.expect('[')?;
        let mut elements = Vec::new();
        self.skip_trivia();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(AppTerm::List(elements));
        }
        loop {
            elements.push(self.term()?);
            self.skip_trivia();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    return Ok(AppTerm::List(elements));
                }
                Some('|') => {
                    return Err(format!(
                        "the improper list tail at byte {} is outside the supported \
                         application-resource subset",
                        self.position
                    ))
                }
                Some(found) => {
                    return Err(format!(
                        "expected `,` or `]` but found {found:?} at byte {}",
                        self.position
                    ))
                }
                None => return Err("the list is not closed".to_string()),
            }
        }
    }

    /// Reads an integer (with an optional `-`).
    fn integer(&mut self) -> std::result::Result<AppTerm, String> {
        let start = self.position;
        if self.peek() == Some('-') {
            self.bump();
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.')
            && self
                .rest()
                .chars()
                .nth(1)
                .is_some_and(|c| c.is_ascii_digit())
        {
            return Err(format!(
                "the float literal at byte {start} is outside the supported \
                 application-resource subset"
            ));
        }
        let text = &self.source[start..self.position];
        text.parse::<i64>()
            .map(AppTerm::Int)
            .map_err(|_| format!("{text:?} at byte {start} is not a 64-bit integer"))
    }

    /// Reads the text between two occurrences of `quote`, resolving `\`-escapes.
    fn quoted(&mut self, quote: char) -> std::result::Result<String, String> {
        let open = self.position;
        self.bump();
        let mut out = String::new();
        loop {
            let Some(character) = self.bump() else {
                return Err(format!("the text opened at byte {open} is not closed"));
            };
            if character == quote {
                return Ok(out);
            }
            if character != '\\' {
                out.push(character);
                continue;
            }
            let escape_at = self.position;
            let Some(escape) = self.bump() else {
                return Err(format!("the escape at byte {escape_at} is incomplete"));
            };
            out.push(match escape {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'v' => '\u{b}',
                'e' => '\u{1b}',
                's' => ' ',
                '\\' => '\\',
                '"' => '"',
                '\'' => '\'',
                other => {
                    return Err(format!(
                        "the escape `\\{other}` at byte {escape_at} is outside the supported \
                         application-resource subset"
                    ))
                }
            });
        }
    }

    /// Reads an unquoted atom (a leading lowercase letter, then letters, digits, `_` or
    /// `@`).
    fn unquoted_atom(&mut self) -> String {
        let start = self.position;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')
        {
            self.bump();
        }
        self.source[start..self.position].to_string()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `application:*` entries (oracle shapes for every BIF) and for
    //! the application-resource reader.
    //!
    //! Every entry that needs to resolve a real installation is gated behind
    //! [`system_runtime`], which skips **loudly** (see `native::test_support`). The
    //! entries that never need one (a non-atom argument is never an application or a key)
    //! run unconditionally.

    use super::*;
    use crate::native::test_support::{atom, call, runtime, string_of, system_runtime};
    use beam_term::make_small;

    /// The `description` every shipping OTP application resource declares (verified
    /// against OTP 29/erts-17.1's `kernel-11.0.4` and `stdlib-8.1`).
    const DESCRIPTION: &str = "ERTS  CXC 138 10";

    /// The 2-tuple `terms::tuple_element` pair, or a test failure naming the answer.
    fn pair(term: Eterm) -> (Eterm, Eterm) {
        assert_eq!(
            terms::tuple_arity(term).expect("a 2-tuple"),
            2,
            "expected a pair, got {term:?}"
        );
        (
            terms::tuple_element(term, 0).expect("element 0"),
            terms::tuple_element(term, 1).expect("element 1"),
        )
    }

    // -- get_env/1 ------------------------------------------------------------

    #[test]
    fn get_env_1_is_always_undefined() {
        // Oracle-verified: `application:get_env(kernel)` ⇒ `undefined`,
        // `application:get_env(42)` ⇒ `undefined`. The `/1` form is the calling process's
        // application, and this runtime runs no application.
        let mut runtime = runtime();
        for argument in [atom(&mut runtime, "kernel"), make_small(42)] {
            let answer =
                call(&mut runtime, "application", "get_env", &[argument]).expect("get_env/1");
            assert_eq!(runtime.atom_name_of(answer), Some("undefined"));
        }
        // It needs no code path at all: the `/1` form never resolves an application.
        let answer = call(&mut runtime, "application", "get_env", &[make_small(1)])
            .expect("get_env/1 without a code path");
        assert_eq!(runtime.atom_name_of(answer), Some("undefined"));
    }

    // -- get_env/2 ------------------------------------------------------------

    #[test]
    fn get_env_2_of_a_non_atom_names_no_application_or_key() {
        // Oracle-verified: `application:get_env(kernel, 42)` ⇒ `undefined`,
        // `application:get_env(42, bogus)` ⇒ `undefined`. Neither argument names anything,
        // so the answer never touches the code path.
        let mut runtime = runtime();
        let kernel = atom(&mut runtime, "kernel");
        let bogus = atom(&mut runtime, "bogus");
        for args in [
            vec![kernel, make_small(42)],
            vec![make_small(42), bogus],
            vec![make_small(42), make_small(42)],
        ] {
            let answer = call(&mut runtime, "application", "get_env", &args).expect("get_env/2");
            assert_eq!(runtime.atom_name_of(answer), Some("undefined"), "{args:?}");
        }
    }

    #[test]
    fn get_env_2_reports_a_declared_value_and_the_not_found_atom() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let kernel = atom(&mut runtime, "kernel");
        let stdlib = atom(&mut runtime, "stdlib");

        // kernel.app declares `{net_ticktime, 60}` in its own `{env, [...]}`.
        let net_ticktime = atom(&mut runtime, "net_ticktime");
        let answer = call(
            &mut runtime,
            "application",
            "get_env",
            &[kernel, net_ticktime],
        )
        .expect("get_env/2");
        let (tag, value) = pair(answer);
        assert_eq!(runtime.atom_name_of(tag), Some("ok"));
        assert_eq!(
            value,
            make_small(60),
            "kernel.app declares net_ticktime = 60"
        );

        // An absent key, an absent application and an application with an empty
        // environment are all `undefined`.
        let nope = atom(&mut runtime, "nope");
        let answer =
            call(&mut runtime, "application", "get_env", &[kernel, nope]).expect("get_env/2");
        assert_eq!(runtime.atom_name_of(answer), Some("undefined"));

        let bogus_app = atom(&mut runtime, "definitely_not_an_otp_app");
        let answer =
            call(&mut runtime, "application", "get_env", &[bogus_app, nope]).expect("get_env/2");
        assert_eq!(runtime.atom_name_of(answer), Some("undefined"));

        // stdlib's environment is empty, so even a key kernel has is absent there.
        let answer =
            call(&mut runtime, "application", "get_env", &[stdlib, kernel]).expect("get_env/2");
        assert_eq!(runtime.atom_name_of(answer), Some("undefined"));
    }

    // -- get_env/3 ------------------------------------------------------------

    #[test]
    fn get_env_3_falls_back_to_the_default() {
        let mut runtime = runtime();
        let kernel = atom(&mut runtime, "kernel");
        let nope = atom(&mut runtime, "nope");
        let default = atom(&mut runtime, "def");
        // A non-atom application/key can never resolve, so the default comes back without
        // a code path (oracle-verified: `application:get_env(kernel, 42, def)` ⇒ `def`).
        for args in [
            vec![kernel, make_small(42), default],
            vec![make_small(42), nope, default],
            vec![make_small(42), make_small(42), default],
        ] {
            let answer = call(&mut runtime, "application", "get_env", &args).expect("get_env/3");
            assert_eq!(answer, default, "{args:?} answers the default");
        }

        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let kernel = atom(&mut runtime, "kernel");
        let stdlib = atom(&mut runtime, "stdlib");
        let nope = atom(&mut runtime, "nope");
        let net_ticktime = atom(&mut runtime, "net_ticktime");
        let default = atom(&mut runtime, "def");

        // The value itself, not `{ok, Value}`.
        let answer = call(
            &mut runtime,
            "application",
            "get_env",
            &[kernel, net_ticktime, default],
        )
        .expect("get_env/3");
        assert_eq!(answer, make_small(60));

        // An absent key and an absent application answer the default.
        let answer = call(
            &mut runtime,
            "application",
            "get_env",
            &[kernel, nope, default],
        )
        .expect("get_env/3");
        assert_eq!(answer, default);
        let bogus_app = atom(&mut runtime, "definitely_not_an_otp_app");
        let answer = call(
            &mut runtime,
            "application",
            "get_env",
            &[bogus_app, nope, default],
        )
        .expect("get_env/3");
        assert_eq!(answer, default);
        let answer = call(
            &mut runtime,
            "application",
            "get_env",
            &[stdlib, kernel, default],
        )
        .expect("get_env/3");
        assert_eq!(answer, default);
    }

    // -- get_all_env/1 --------------------------------------------------------

    #[test]
    fn get_all_env_1_of_a_non_atom_is_empty() {
        // Oracle-verified: `application:get_all_env(42)` ⇒ `[]`.
        let mut runtime = runtime();
        let answer = call(
            &mut runtime,
            "application",
            "get_all_env",
            &[make_small(42)],
        )
        .expect("get_all_env/1");
        assert!(terms::list_elements(answer).expect("a list").is_empty());
    }

    #[test]
    fn get_all_env_1_reports_the_declared_environment() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let stdlib = atom(&mut runtime, "stdlib");
        let bogus = atom(&mut runtime, "definitely_not_an_otp_app");

        // stdlib declares `{env, []}`.
        let answer =
            call(&mut runtime, "application", "get_all_env", &[stdlib]).expect("get_all_env/1");
        assert!(
            terms::list_elements(answer).expect("a list").is_empty(),
            "stdlib's declared environment is empty"
        );
        // An unresolvable application is empty too.
        let answer =
            call(&mut runtime, "application", "get_all_env", &[bogus]).expect("get_all_env/1");
        assert!(terms::list_elements(answer).expect("a list").is_empty());

        // kernel declares its own environment; `net_ticktime` is one of its pairs.
        let kernel = atom(&mut runtime, "kernel");
        let answer =
            call(&mut runtime, "application", "get_all_env", &[kernel]).expect("get_all_env/1");
        let entries = terms::list_elements(answer).expect("a list");
        assert!(
            !entries.is_empty(),
            "kernel.app declares an environment, got {entries:?}"
        );
        let net_ticktime = entries
            .iter()
            .find_map(|entry| {
                let (key, value) = pair(*entry);
                (runtime.atom_name_of(key) == Some("net_ticktime")).then_some(value)
            })
            .expect("kernel.app declares net_ticktime");
        assert_eq!(
            net_ticktime,
            make_small(60),
            "the declared value is the file's own 60"
        );

        // The declared key **set** is exactly kernel.app's `{env, [...]}` — seven entries,
        // in the file's order. The oracle reports nine: its application controller injects
        // `logger` (the default handler config kernel's own start installs) and
        // `connect_all` (set on the distribution/global startup) at run time. This runtime
        // runs no application start, so those two are honestly absent — never fabricated.
        // Oracle (OTP 29/erts-17.1):
        //   `length(application:get_all_env(kernel))` ⇒ 9
        //   `application:get_env(kernel, logger)` ⇒ `{ok, [{handler, default, …}]}`
        //   `application:get_env(kernel, connect_all)` ⇒ `{ok, true}`
        let keys: Vec<&str> = entries
            .iter()
            .map(|entry| {
                runtime
                    .atom_name_of(pair(*entry).0)
                    .expect("a declared key atom")
            })
            .collect();
        assert_eq!(keys.len(), 7, "kernel.app declares seven env entries: {keys:?}");
        for declared in [
            "logger_level",
            "logger_sasl_compatible",
            "net_tickintensity",
            "net_ticktime",
            "prevent_overlapping_partitions",
            "shell_docs_ansi",
            "shell_history_drop",
        ] {
            assert!(keys.contains(&declared), "{declared} is declared: {keys:?}");
        }
        for injected in ["logger", "connect_all"] {
            assert!(
                !keys.contains(&injected),
                "{injected} is injected by the oracle's running application, not declared"
            );
        }
    }

    // -- loaded_applications/0 and which_applications/0 ------------------------

    /// The `[{Application, Description, Vsn}]` triples of `function/0` under `runtime`,
    /// as `(application, description, vsn)` string triples.
    fn triples(runtime: &mut crate::Runtime, function: &str) -> Vec<(String, String, String)> {
        let answer = call(runtime, "application", function, &[]).expect(function);
        terms::list_elements(answer)
            .expect("a list")
            .iter()
            .map(|entry| {
                assert_eq!(
                    terms::tuple_arity(*entry).expect("a 3-tuple"),
                    3,
                    "every entry is an {{Application, Description, Vsn}} triple"
                );
                let name = terms::tuple_element(*entry, 0).expect("the application");
                let description = terms::tuple_element(*entry, 1).expect("the description");
                let vsn = terms::tuple_element(*entry, 2).expect("the version");
                (
                    runtime
                        .atom_name_of(name)
                        .expect("the application is an atom")
                        .to_string(),
                    string_of(description),
                    string_of(vsn),
                )
            })
            .collect()
    }

    #[test]
    fn loaded_applications_pins_kernel_then_stdlib() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        // Oracle-verified (OTP 29/erts-17.1):
        // `[{kernel,"ERTS  CXC 138 10","11.0.4"},{stdlib,"ERTS  CXC 138 10","8.1"}]`.
        assert_eq!(
            triples(&mut runtime, "loaded_applications"),
            vec![
                (
                    "kernel".to_string(),
                    DESCRIPTION.to_string(),
                    "11.0.4".to_string()
                ),
                (
                    "stdlib".to_string(),
                    DESCRIPTION.to_string(),
                    "8.1".to_string()
                ),
            ]
        );
    }

    #[test]
    fn which_applications_is_the_reverse_of_loaded_applications() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        // Oracle-verified: `which_applications/0` reports the same pair **reversed**.
        let loaded = triples(&mut runtime, "loaded_applications");
        let which = triples(&mut runtime, "which_applications");
        assert_eq!(which, loaded.into_iter().rev().collect::<Vec<_>>());
        assert_eq!(
            which,
            vec![
                (
                    "stdlib".to_string(),
                    DESCRIPTION.to_string(),
                    "8.1".to_string()
                ),
                (
                    "kernel".to_string(),
                    DESCRIPTION.to_string(),
                    "11.0.4".to_string()
                ),
            ]
        );
    }

    #[test]
    fn an_unresolvable_application_contributes_no_triple() {
        let mut runtime = runtime();

        // The resolver's contract: an application the code path cannot resolve has no
        // properties — that is exactly what makes it contribute no triple, rather than a
        // fabricated one.
        {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut runtime, &mut sink);
            assert!(context
                .app_props("definitely_not_an_otp_app")
                .expect("an unresolvable application is an honest `None`")
                .is_none());
        }

        // And every triple that *is* reported belongs to an application that really
        // resolves: there is no path that produces one for an application that is not
        // installed. A runtime without a system Erlang resolves nothing, so the list is
        // honestly empty and the assertion holds vacuously — the honest empty answer is
        // the other half of the same contract.
        for (app, _, _) in triples(&mut runtime, "loaded_applications") {
            assert!(
                runtime.code_server.app_dir(&app).is_some(),
                "{app} is reported as loaded, so it must resolve"
            );
        }
    }

    // -- get_key/2 ------------------------------------------------------------

    #[test]
    fn get_key_2_of_a_non_atom_names_no_application_or_key() {
        // Oracle-verified: `application:get_key(42, vsn)` ⇒ `undefined`.
        let mut runtime = runtime();
        let vsn = atom(&mut runtime, "vsn");
        let answer = call(
            &mut runtime,
            "application",
            "get_key",
            &[make_small(42), vsn],
        )
        .expect("get_key/2");
        assert_eq!(runtime.atom_name_of(answer), Some("undefined"));

        let kernel = atom(&mut runtime, "kernel");
        let answer = call(
            &mut runtime,
            "application",
            "get_key",
            &[kernel, make_small(42)],
        )
        .expect("get_key/2");
        assert_eq!(runtime.atom_name_of(answer), Some("undefined"));
    }

    #[test]
    fn get_key_2_answers_any_proplist_key_or_undefined() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let kernel = atom(&mut runtime, "kernel");
        let stdlib = atom(&mut runtime, "stdlib");

        // `{ok, Value}` for a declared string property.
        for (key, expected) in [("vsn", "11.0.4"), ("description", DESCRIPTION)] {
            let key_atom = atom(&mut runtime, key);
            let answer = call(&mut runtime, "application", "get_key", &[kernel, key_atom])
                .expect("get_key/2");
            let (tag, value) = pair(answer);
            assert_eq!(runtime.atom_name_of(tag), Some("ok"));
            assert_eq!(string_of(value), expected, "get_key(kernel, {key})");
        }

        // A declared list-of-atoms property: stdlib's `{applications, [kernel]}`.
        let applications = atom(&mut runtime, "applications");
        let answer = call(
            &mut runtime,
            "application",
            "get_key",
            &[stdlib, applications],
        )
        .expect("get_key/2");
        let (tag, value) = pair(answer);
        assert_eq!(runtime.atom_name_of(tag), Some("ok"));
        let entries = terms::list_elements(value).expect("a list");
        assert_eq!(entries.len(), 1, "stdlib depends on kernel alone");
        assert_eq!(runtime.atom_name_of(entries[0]), Some("kernel"));

        // A declared list-of-module-atoms property: the whole `modules` property.
        let modules = atom(&mut runtime, "modules");
        let answer =
            call(&mut runtime, "application", "get_key", &[kernel, modules]).expect("get_key/2");
        let (tag, value) = pair(answer);
        assert_eq!(runtime.atom_name_of(tag), Some("ok"));
        let names: Vec<&str> = terms::list_elements(value)
            .expect("a list of module atoms")
            .iter()
            .map(|name| runtime.atom_name_of(*name).expect("a module atom"))
            .collect();
        for expected in ["application", "code", "file", "kernel"] {
            assert!(names.contains(&expected), "kernel.app lists {expected}");
        }

        // An absent key and an unresolvable application are `undefined`.
        let nope_key = atom(&mut runtime, "nope_key");
        for app in [kernel, atom(&mut runtime, "definitely_not_an_otp_app")] {
            let answer =
                call(&mut runtime, "application", "get_key", &[app, nope_key]).expect("get_key/2");
            assert_eq!(runtime.atom_name_of(answer), Some("undefined"));
        }
    }

    // -- get_application/0 ----------------------------------------------------

    #[test]
    fn get_application_0_is_undefined() {
        // Oracle-verified (OTP 29/erts-17.1): `application:get_application()` ⇒
        // `undefined`. The `/0` form asks for the *calling process's* application
        // (`application.erl:1007-1008`), and this runtime runs no application controller,
        // so no process belongs to an application — the same reason `get_env/1` is
        // `undefined` for every key.
        //
        // Driven through the entry itself rather than `call`: `application:get_application/0`
        // has no `try_native` dispatch arm yet (the registration lives in `src/native.rs`,
        // outside this node — see `src/native/CONTEXT.md`).
        let mut runtime = runtime();
        let answer = {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut runtime, &mut sink);
            context
                .application_get_application_0()
                .expect("application:get_application/0")
        };
        assert_eq!(runtime.atom_name_of(answer), Some("undefined"));
    }

    // -- the application-resource reader --------------------------------------

    /// The parsed value, or a test failure naming the read error.
    fn parse(source: &str) -> AppTerm {
        parse_resource(source).unwrap_or_else(|error| panic!("{source:?}: {error}"))
    }

    #[test]
    fn reads_the_shapes_of_a_shipping_application_resource() {
        // A trimmed version of the real files: comments, atoms, strings, integers, nested
        // lists and tuples.
        let source = "\
%% This is an -*- erlang -*- file.\n\
%% a second comment line\n\
{application, kernel,\n\
 [\n\
  {description, \"ERTS  CXC 138 10\"},\n\
  {vsn, \"11.0.4\"},\n\
  {modules, [application, code, % a trailing comment\n\
             'quoted-atom', kernel]},\n\
  {applications, []},\n\
  {env, [{net_ticktime, 60}, {shell_docs_ansi, auto}]},\n\
  {nil, []},\n\
  {negative, -3}\n\
 ]}.\n";
        let AppTerm::Tuple(outer) = parse(source) else {
            panic!("the resource is a tuple");
        };
        assert_eq!(outer.len(), 3);
        let AppTerm::Atom(tag) = &outer[0] else {
            panic!("the tag is an atom");
        };
        assert_eq!(tag, "application");
        assert_eq!(outer[1], AppTerm::Atom("kernel".to_string()));
        // The trailing `.` is checked separately: it lives *outside* the term.
        let AppTerm::List(props) = &outer[2] else {
            panic!("the properties are a list");
        };
        assert_eq!(props.len(), 7);
        assert_eq!(
            props[0],
            AppTerm::Tuple(vec![
                AppTerm::Atom("description".to_string()),
                AppTerm::Str(DESCRIPTION.to_string()),
            ])
        );
        assert_eq!(
            props[2],
            AppTerm::Tuple(vec![
                AppTerm::Atom("modules".to_string()),
                AppTerm::List(vec![
                    AppTerm::Atom("application".to_string()),
                    AppTerm::Atom("code".to_string()),
                    AppTerm::Atom("quoted-atom".to_string()),
                    AppTerm::Atom("kernel".to_string()),
                ]),
            ])
        );
        assert_eq!(
            props[3],
            AppTerm::Tuple(vec![
                AppTerm::Atom("applications".to_string()),
                AppTerm::List(Vec::new()),
            ])
        );
        assert_eq!(
            props[4],
            AppTerm::Tuple(vec![
                AppTerm::Atom("env".to_string()),
                AppTerm::List(vec![
                    AppTerm::Tuple(vec![
                        AppTerm::Atom("net_ticktime".to_string()),
                        AppTerm::Int(60),
                    ]),
                    AppTerm::Tuple(vec![
                        AppTerm::Atom("shell_docs_ansi".to_string()),
                        AppTerm::Atom("auto".to_string()),
                    ]),
                ]),
            ])
        );
        assert_eq!(
            props[5],
            AppTerm::Tuple(vec![
                AppTerm::Atom("nil".to_string()),
                AppTerm::List(Vec::new()),
            ])
        );
        assert_eq!(
            props[6],
            AppTerm::Tuple(vec![
                AppTerm::Atom("negative".to_string()),
                AppTerm::Int(-3),
            ])
        );
    }

    #[test]
    fn reads_an_empty_tuple_and_resolves_string_escapes() {
        let AppTerm::Tuple(elements) = parse("{ }") else {
            panic!("an empty tuple");
        };
        assert!(elements.is_empty());
        assert_eq!(
            parse("\"a\\nb\\t\\\\\\\"c\""),
            AppTerm::Str("a\nb\t\\\"c".to_string())
        );
        assert_eq!(parse("'it\\'s'"), AppTerm::Atom("it's".to_string()));
        // A comment may run to the end of the input, and whitespace is insignificant.
        assert_eq!(
            parse("  kernel  %% trailing"),
            AppTerm::Atom("kernel".to_string())
        );
    }

    #[test]
    fn reports_every_construct_outside_the_subset() {
        // Each case names what it rejected, and none of them is silently mis-read.
        for (source, expected) in [
            ("1.5", "float literal"),
            (
                "<<1,2>>",
                "outside the supported application-resource subset",
            ),
            ("[a | b]", "improper list tail"),
            (
                "#{a => 1}",
                "outside the supported application-resource subset",
            ),
            ("$a", "outside the supported application-resource subset"),
            ("\"unclosed", "is not closed"),
            ("{a", "the tuple is not closed"),
            ("[a", "the list is not closed"),
            ("{a b}", "expected `,` or `}`"),
            ("[a b]", "expected `,` or `]"),
            ("{a,}", "outside the supported application-resource subset"),
            ("\"a\\q\"", "the escape `\\q`"),
            ("9223372036854775808", "not a 64-bit integer"),
            ("", "a term was expected but the input ended"),
        ] {
            let error =
                parse_resource(source).expect_err(&format!("{source:?} is outside the subset"));
            assert!(error.contains(expected), "{source:?}: {error}");
        }
        // Trailing text after the term is a read error, not silently ignored.
        let error = parse_resource("kernel junk").expect_err("trailing text");
        assert!(error.contains("trailing text"), "{error}");
    }

    #[test]
    fn materializes_parsed_terms_and_rejects_a_non_small_integer() {
        let mut runtime = runtime();
        let term = {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut runtime, &mut sink);
            let term = context
                .materialize(&parse("[kernel, \"hi\", {vsn, 8}]"))
                .expect("materialize");
            // The 60-bit immediate range is the term layout's limit: a larger integer
            // would need a bignum, which this reader does not build. A typed limit,
            // never a wrong value.
            for out_of_range in [i64::MAX, i64::MIN] {
                assert!(
                    matches!(
                        context.materialize(&AppTerm::Int(out_of_range)),
                        Err(BeamError::SystemLimit)
                    ),
                    "{out_of_range} is outside the small-int range"
                );
            }
            assert!(context.materialize(&AppTerm::Int(60)).is_ok());
            term
        };

        // An atom is interned, a string is a char list, a list/tuple is built recursively.
        let entries = terms::list_elements(term).expect("a list");
        assert_eq!(entries.len(), 3);
        assert_eq!(runtime.atom_name_of(entries[0]), Some("kernel"));
        assert_eq!(string_of(entries[1]), "hi");
        let (key, value) = pair(entries[2]);
        assert_eq!(runtime.atom_name_of(key), Some("vsn"));
        assert_eq!(value, make_small(8));
    }

    #[test]
    fn an_existing_but_unreadable_resource_is_a_typed_error() {
        // The reader is exercised through the floor's own resolution: a *directory* named
        // like a resource file (so it "exists" but cannot be read) must be reported, never
        // silently treated as an unresolvable application.
        let dir =
            std::env::temp_dir().join(format!("beam_rt_unreadable_app_{}", std::process::id()));
        // `app_dir` matches an application by the **name of the directory the `ebin` lives
        // in**, so the fake application is named after the scratch directory itself.
        let app = dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a directory name")
            .to_string();
        let ebin = dir.join("ebin");
        // A *directory* in the resource file's place: the file "is there" but cannot be
        // read, which must be reported rather than treated as an unresolvable application.
        std::fs::create_dir_all(ebin.join(format!("{app}.app")))
            .expect("create the fake resource directory");
        // Seed the code path with the fake application's `ebin` so `app_dir` resolves it.
        let config = crate::config::RuntimeConfig::default().with_code_path(&ebin);
        let mut runtime = crate::Runtime::new(config).expect("a runtime over the fake ebin");
        let app_atom = atom(&mut runtime, &app);
        let vsn = atom(&mut runtime, "vsn");
        let error = call(&mut runtime, "application", "get_key", &[app_atom, vsn])
            .expect_err("a directory is not a readable resource file");
        assert!(
            error.to_string().contains("cannot read"),
            "the read failure is reported: {error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
