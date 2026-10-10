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

//! The `init:*` runtime-native floor: the parsed command line (contract A.6) and the boot
//! status (contract A.7).
//!
//! `init` is a **preloaded** module: the oracle serves these calls from `init.beam`, which
//! is written against the process/message machinery Milestone-1 does not run. The runtime
//! therefore answers them directly from its own state — the parsed
//! [`RuntimeConfig::init_args`](crate::config::InitArgs) for the command line, the
//! terminal boot state for `get_status/0` — instead of loading a module it cannot execute
//! (see the parent module's docs).

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::Eterm;

use crate::context::RuntimeContext;
use crate::error;

impl RuntimeContext<'_> {
    /// `init:get_argument(Flag)`: `{ok, [[Value…]…]}` for a flag that was given (one
    /// inner list per occurrence, empty for a valueless flag), or the atom `error`.
    ///
    /// Four flags are **automatic** — the emulator defines them itself, so they are not
    /// command-line flags and have no [`InitArgs`](crate::config::InitArgs) entry
    /// (`init.erl:363-388`): `root` (the installation directory, served from the same
    /// resolution as [`code:root_dir/0`](Self::code_root_dir)), `progname` (the name of
    /// the program that started the runtime — see [`progname`]), `home` (the `$HOME`
    /// environment variable) and `bindir` (the emulator's `bin` directory —
    /// [`CodeServer::bindir`](crate::CodeServer::bindir)). These are answered from the
    /// runtime's own state; a flag the runtime has no value for is the atom `error`,
    /// exactly like an absent flag.
    ///
    /// The occurrences are **snapshotted** out of the configuration before any term is
    /// built: every occurrence is owned data by then, so no borrow of the runtime is
    /// held across the mutable term construction below.
    pub(super) fn get_argument(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let flag = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        match flag.as_str() {
            // The code root is a hard error when the system installation cannot be
            // resolved at all (the same diagnostic `code:root_dir/0` reports) — an
            // unresolvable installation is not "no root argument", it is a broken boot.
            "root" => {
                let root = self.code_root_dir()?;
                return self.ok_single_value(root);
            }
            // `$HOME` and its UTF-8 requirement: the oracle reports the value of the
            // environment variable decoded to a char list, and `error` when it is unset.
            // A home directory whose bytes are not valid UTF-8 has no char-list image in
            // this runtime, so it is reported as `error` rather than a lossily mangled
            // path.
            "home" => {
                return match std::env::var("HOME") {
                    Ok(home) => {
                        let value = self.charlist(&home)?;
                        self.ok_single_value(value)
                    }
                    Err(_) => self.atom("error"),
                };
            }
            "progname" => {
                return match progname() {
                    Some(name) => {
                        let value = self.charlist(&name)?;
                        self.ok_single_value(value)
                    }
                    None => self.atom("error"),
                };
            }
            // `bindir` — the directory holding the emulator executables — is a
            // *filesystem* fact (`erlexec.c:1541-1543`): `<rootdir>/erts-<vsn>/bin`. It is
            // derived from the very same installation resolution the code path uses; when
            // no installation (and so no emulator directory) is resolvable there is no
            // bindir to report, so the honest answer is `error` (never a fabricated
            // directory).
            "bindir" => {
                let _ = self.runtime.code_server.add_system_paths();
                return match self.runtime.code_server.bindir() {
                    Some(dir) => {
                        let value = self.charlist(&dir.display().to_string())?;
                        self.ok_single_value(value)
                    }
                    None => self.atom("error"),
                };
            }
            _ => {}
        }

        let occurrences: Vec<Vec<String>> = match self.runtime.init_args().get(&flag) {
            Some(list) => list.into_iter().map(<[String]>::to_vec).collect(),
            None => return self.atom("error"),
        };

        let mut occurrence_terms = Vec::with_capacity(occurrences.len());
        for occurrence in &occurrences {
            let mut value_terms = Vec::with_capacity(occurrence.len());
            for value in occurrence {
                value_terms.push(self.charlist(value)?);
            }
            occurrence_terms.push(terms::make_list(self, &value_terms)?);
        }
        let values = terms::make_list(self, &occurrence_terms)?;
        let ok = self.atom("ok")?;
        terms::make_tuple(self, &[ok, values])
    }

    /// `{ok, [[Value]]}` — the `init:get_argument/1` answer for an automatic flag that
    /// holds exactly one occurrence of exactly one value (`root`, `progname`, `home`).
    fn ok_single_value(&mut self, value: Eterm) -> Result<Eterm> {
        let occurrence = terms::make_list(self, &[value])?;
        let values = terms::make_list(self, &[occurrence])?;
        let ok = self.atom("ok")?;
        terms::make_tuple(self, &[ok, values])
    }

    /// `init:get_plain_arguments/0`: the arguments the emulator did not model, each as a
    /// string (contract A.6).
    ///
    /// This runtime has no `-extra` handling and the CLI rejects a stray positional
    /// argument outright (exit 2, see [`crate::cli::parse`]), so no plain argument can
    /// ever have been given: the result is always `[]` — which is exactly what the
    /// oracle reports for the corpus argv `["-noshell", …, "-eval", …]` (verified
    /// against OTP 29/erts-17.1).
    pub(super) fn get_plain_arguments(&mut self) -> Result<Eterm> {
        Ok(beam_term::NIL)
    }

    /// `init:get_arguments/0`: the whole command line as `{Flag, [Value…]}` pairs, in
    /// the order the flags were given (contract A.6).
    ///
    /// Built from [`RuntimeConfig::init_args`](crate::config::InitArgs), the same source
    /// as `init:get_argument/1`: a bare flag's value list is empty, and each value is a
    /// string (a char list). The oracle *additionally* reports the emulator's own
    /// `root`/`bindir`/`progname`/`home` entries; those are not command-line flags, so
    /// they do not belong to this listing (`init:get_argument/1` answers `root`,
    /// `progname` and `home` from the runtime's own state — see [`Self::get_argument`] —
    /// while `bindir` is not modelled). A `get_arguments/0` comparison against the oracle
    /// is therefore expected to differ by exactly those four.
    pub(super) fn get_arguments(&mut self) -> Result<Eterm> {
        // Snapshot first: every value is owned data by then, so no borrow of the
        // runtime is held across the mutable term construction below.
        let occurrences: Vec<(String, Vec<String>)> = self
            .runtime
            .init_args()
            .occurrences()
            .map(|(flag, values)| (flag.to_string(), values.to_vec()))
            .collect();

        let mut pairs = Vec::with_capacity(occurrences.len());
        for (flag, values) in &occurrences {
            let flag_atom = self.atom(flag)?;
            let value_list = self.string_list(values)?;
            pairs.push(terms::make_tuple(self, &[flag_atom, value_list])?);
        }
        terms::make_list(self, &pairs)
    }

    /// `init:get_status/0`: the boot-status 2-tuple `{starting, started}`.
    ///
    /// The oracle's `init.beam` reports the state transition of the boot process; a
    /// runtime that has reached a point where code is running has completed it, so the
    /// value is the terminal state — verified against OTP 29/erts-17.1
    /// (`io:format("~p~n", [init:get_status()])` → `{starting,started}`).
    ///
    /// This is preloaded-module surface (contract A.7): `init` is preloaded and its
    /// `.beam` is not run (see the parent module's docs), so the constant is answered
    /// here rather than by loading a module this runtime cannot execute.
    pub(super) fn get_status(&mut self) -> Result<Eterm> {
        let starting = self.atom("starting")?;
        let started = self.atom("started")?;
        terms::make_tuple(self, &[starting, started])
    }

    /// `init:script_id/0`: the `{Name, Vsn}` header of the **loaded** boot script, as a
    /// 2-tuple of strings (contracts A.7/A.9).
    ///
    /// The oracle's `init` answers the id of the script it booted (`init.erl`'s
    /// `script_id/0` returns the `Id` it read out of the `.boot` file). This runtime
    /// records that header when it loads a script ([`Runtime::boot_from_script`]); before
    /// any script has been loaded there is no id to report, which is a typed capability
    /// gap — never a fabricated `{"Erlang/OTP", …}` literal.
    ///
    /// `// documented divergence:` this runtime's release is OTP `30.0-rc0`, so the header
    /// of the committed `bootstrap/bin/start.boot` it loads is `{"Erlang/OTP","30"}` where
    /// the OTP-29 oracle reports `{"Erlang/OTP","29"}`. The value is the *real* header of
    /// the script this runtime loaded, so it tracks the release exactly as the oracle's
    /// does — the difference is the release, not the mechanism.
    ///
    /// [`Runtime::boot_from_script`]: crate::Runtime::boot_from_script
    pub(super) fn script_id_0(&mut self) -> Result<Eterm> {
        let Some((name, vsn)) = self
            .runtime
            .boot_script_id()
            .map(|(name, vsn)| (name.to_string(), vsn.to_string()))
        else {
            return Err(error::unsupported(
                "init:script_id/0 (no boot script has been loaded)",
            ));
        };
        let name = self.charlist(&name)?;
        let vsn = self.charlist(&vsn)?;
        terms::make_tuple(self, &[name, vsn])
    }
}

/// The name of the program that started this runtime — the value of
/// `init:get_argument(progname)`.
///
/// The C emulator composes it in exactly one place (`erts/etc/common/erlexec.c:1702-1705`):
/// `progname = get_env("PROGNAME")`, falling back to the compile-time
/// `DEFAULT_PROGNAME = "erl"` (`erlexec.c:36`). It is **not** the basename of `$0` — the
/// `erl` shell script is what exports `PROGNAME` before starting the emulator, so an
/// unset `PROGNAME` always yields `"erl"` however the program was invoked. This runtime
/// reproduces the C rule verbatim.
///
/// `None` only when `PROGNAME` is set to a value this runtime's char lists cannot carry
/// (not valid UTF-8); the caller then answers `error`, exactly as for an unset variable.
fn progname() -> Option<String> {
    progname_from(std::env::var_os("PROGNAME").as_deref())
}

/// The C emulator's compile-time `progname` default (`erlexec.c:36`, `:1702-1705`).
const DEFAULT_PROGNAME: &str = "erl";

/// The `progname` value for a given `PROGNAME` environment value — the pure rule behind
/// [`progname`]: a set, non-empty value wins, anything else is [`DEFAULT_PROGNAME`], and a
/// set-but-non-UTF-8 value has no char-list image (`None`).
fn progname_from(env: Option<&std::ffi::OsStr>) -> Option<String> {
    match env {
        Some(value) if !value.is_empty() => value.to_str().map(str::to_string),
        _ => Some(DEFAULT_PROGNAME.to_string()),
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `init:*` entries: the command-line surface
    //! (`get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`), the emulator's
    //! automatic flags (`root`/`home`/`progname`) and the boot status.

    use super::*;
    use crate::native::test_support::{atom, call, runtime, string_of, system_runtime};
    use crate::runtime::Runtime;

    #[test]
    fn get_argument_reports_occurrences_and_absent_flags() {
        let mut config = crate::config::RuntimeConfig::default();
        config.init_args.record("noshell", Vec::new());
        config.init_args.record("pa", vec!["/a".to_string()]);
        config.init_args.record("pa", vec!["/b".to_string()]);
        let mut runtime = Runtime::new(config).expect("a runtime with init args");

        let noshell = atom(&mut runtime, "noshell");
        let present = call(&mut runtime, "init", "get_argument", &[noshell]).expect("present");
        // `{ok, [[]]}`: one occurrence, no values.
        assert_eq!(terms::tuple_arity(present).expect("a 2-tuple"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(present, 0).unwrap()),
            Some("ok")
        );
        let values = terms::tuple_element(present, 1).unwrap();
        let occurrences = terms::list_elements(values).expect("the occurrence list");
        assert_eq!(occurrences.len(), 1);
        assert!(terms::list_elements(occurrences[0])
            .expect("no values")
            .is_empty());

        let pa = atom(&mut runtime, "pa");
        let present = call(&mut runtime, "init", "get_argument", &[pa]).expect("present");
        let values = terms::tuple_element(present, 1).unwrap();
        let occurrences = terms::list_elements(values).expect("the occurrence list");
        assert_eq!(occurrences.len(), 2);
        // Each occurrence is a list of **strings** (the oracle's
        // `{ok,[[[47,97]],[[47,98]]]}` for `-pa /a -pa /b`), so occurrence 0 is
        // `["/a"]` and its single element is the char list `"/a"`.
        let occurrence = terms::list_elements(occurrences[0]).expect("one value");
        assert_eq!(occurrence.len(), 1, "one `-pa` value");
        assert_eq!(
            terms::list_elements(occurrence[0]).expect("the value as a string"),
            vec![
                beam_term::make_small(i64::from(b'/')),
                beam_term::make_small(i64::from(b'a'))
            ]
        );

        let absent = atom(&mut runtime, "absent");
        let missing = call(&mut runtime, "init", "get_argument", &[absent]).expect("absent");
        assert_eq!(runtime.atom_name_of(missing), Some("error"));
    }

    #[test]
    fn get_plain_arguments_is_always_empty() {
        // The CLI models no plain arguments (a stray positional argument is a usage
        // error), so `init:get_plain_arguments/0` is the empty list, as the oracle's is.
        let mut runtime = runtime();
        let plain = call(&mut runtime, "init", "get_plain_arguments", &[]).expect("plain args");
        assert_eq!(plain, beam_term::NIL);
        assert!(terms::list_elements(plain).expect("a list").is_empty());
    }

    #[test]
    fn get_arguments_reports_every_flag_in_order() {
        let mut config = crate::config::RuntimeConfig::default();
        config.init_args.record("noshell", Vec::new());
        config.init_args.record("pa", vec!["/a".to_string()]);
        config.init_args.record("pa", vec!["/b".to_string(), "/c".to_string()]);
        let mut runtime = Runtime::new(config).expect("a runtime with init args");

        let all = call(&mut runtime, "init", "get_arguments", &[]).expect("get_arguments/0");
        let entries = terms::list_elements(all).expect("a list of pairs");
        let pairs: Vec<(String, Vec<String>)> = entries
            .iter()
            .map(|entry| {
                assert_eq!(terms::tuple_arity(*entry).expect("a pair"), 2);
                let flag = terms::tuple_element(*entry, 0).expect("a flag atom");
                let values = terms::tuple_element(*entry, 1).expect("a value list");
                (
                    runtime
                        .atom_name_of(flag)
                        .expect("the flag is an atom")
                        .to_string(),
                    terms::list_elements(values)
                        .expect("a value list")
                        .iter()
                        .map(|value| string_of(*value))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("noshell".to_string(), Vec::new()),
                ("pa".to_string(), vec!["/a".to_string()]),
                ("pa".to_string(), vec!["/b".to_string(), "/c".to_string()]),
            ]
        );
    }

    #[test]
    fn get_arguments_of_a_bare_runtime_is_empty() {
        let mut runtime = runtime();
        let all = call(&mut runtime, "init", "get_arguments", &[]).expect("get_arguments/0");
        assert!(terms::list_elements(all).expect("a list").is_empty());
    }

    #[test]
    fn get_status_is_the_two_atom_boot_tuple() {
        // Verified against the oracle: `init:get_status()` is `{starting,started}`, and it
        // must not fall through to loading `init.beam` (which this runtime cannot run).
        let mut runtime = runtime();
        let status = call(&mut runtime, "init", "get_status", &[]).expect("init:get_status/0");
        assert_eq!(terms::tuple_arity(status).expect("a 2-tuple"), 2);
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(status, 0).unwrap()),
            Some("starting")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(status, 1).unwrap()),
            Some("started")
        );
    }

    // -- the emulator's automatic flags (root / home / progname) --------------

    /// The single value of the `{ok, [[Value]]}` answer `init:get_argument/1` gives for
    /// one of the emulator's automatic flags, or `None` when the answer is not that shape
    /// (the atom `error`, or a deeper nesting).
    fn single_automatic_value(runtime: &mut Runtime, term: Eterm) -> Option<String> {
        if runtime.atom_name_of(terms::tuple_element(term, 0).ok()?) != Some("ok") {
            return None;
        }
        let occurrences = terms::list_elements(terms::tuple_element(term, 1).ok()?).ok()?;
        if occurrences.len() != 1 {
            return None;
        }
        let values = terms::list_elements(occurrences[0]).ok()?;
        if values.len() != 1 {
            return None;
        }
        Some(string_of(values[0]))
    }

    #[test]
    fn get_argument_root_is_the_code_root_dir() {
        // `init:get_argument(root)` is the installation directory the system booted from
        // — the same one `code:root_dir/0` reports (oracle: `{ok,[["<root>"]]}`, e.g.
        // `{ok,[["/usr/local/lib/erlang"]]}`). Both come from one resolution, so they can
        // never disagree.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let root = call(&mut runtime, "code", "root_dir", &[]).expect("code:root_dir/0");
        let flag = atom(&mut runtime, "root");
        let answer = call(&mut runtime, "init", "get_argument", &[flag]).expect("root");
        assert_eq!(
            single_automatic_value(&mut runtime, answer).as_deref(),
            Some(string_of(root).as_str()),
            "root is the code root directory"
        );
        // It is a real directory, not the atom `error`.
        assert!(beam_term::is_list(root));
    }

    #[test]
    fn get_argument_home_is_the_home_environment_variable() {
        // Oracle: `init:get_argument(home)` is the value of `$HOME` (`init.erl:380`), or
        // the atom `error` when it is unset. The environment is process-global, so the
        // unset case cannot be exercised from a test; the set case is pinned here.
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let mut runtime = runtime();
        let flag = atom(&mut runtime, "home");
        let answer = call(&mut runtime, "init", "get_argument", &[flag]).expect("home");
        assert_eq!(
            single_automatic_value(&mut runtime, answer).as_deref(),
            Some(home.as_str()),
            "home is `$HOME`"
        );
    }

    #[test]
    fn progname_follows_the_c_env_rule() {
        // `progname = get_env("PROGNAME")`, else the compile-time default `"erl"`
        // (`erlexec.c:36`, `:1702-1705`) — never the invocation basename.
        use std::ffi::OsStr;
        assert_eq!(progname_from(None).as_deref(), Some(DEFAULT_PROGNAME));
        assert_eq!(
            progname_from(Some(OsStr::new(""))).as_deref(),
            Some(DEFAULT_PROGNAME)
        );
        assert_eq!(
            progname_from(Some(OsStr::new("myerl"))).as_deref(),
            Some("myerl")
        );
        // A non-UTF-8 value has no char-list image.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert_eq!(progname_from(Some(OsStr::from_bytes(b"\xff"))), None);
        }
    }

    #[test]
    fn get_argument_progname_is_erl_by_default() {
        // Oracle: `init:get_argument(progname)` ⇒ `{ok,[["erl"]]}` when `PROGNAME` is
        // unset (the `erl` wrapper script is what sets it to anything else). The process
        // environment is global, so the `PROGNAME`-set case is covered by the pure rule
        // above; here the unset case is pinned.
        if std::env::var_os("PROGNAME").is_some() {
            return; // cannot exercise the default with the variable set
        }
        let mut runtime = runtime();
        let flag = atom(&mut runtime, "progname");
        let answer = call(&mut runtime, "init", "get_argument", &[flag]).expect("progname");
        assert_eq!(
            single_automatic_value(&mut runtime, answer).as_deref(),
            Some(DEFAULT_PROGNAME),
            "an unset PROGNAME answers the compile-time default"
        );
    }

    #[test]
    fn get_argument_bindir_is_the_emulator_directory() {
        // Oracle: `init:get_argument(bindir)` ⇒ `{ok,[["<root>/erts-<vsn>/bin"]]}` — the
        // directory holding the emulator executables. It is derived from the installation
        // this runtime's code path resolved, and is a real directory (or `error` when no
        // installation is discoverable, which this test skips over honestly).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let flag = atom(&mut runtime, "bindir");
        let answer = call(&mut runtime, "init", "get_argument", &[flag]).expect("bindir");
        let Some(dir) = single_automatic_value(&mut runtime, answer) else {
            return; // no emulator directory under the resolved root
        };
        assert!(
            std::path::Path::new(&dir).join("beam.smp").is_file(),
            "bindir holds the emulator executables: {dir}"
        );
    }

    #[test]
    fn script_id_is_the_loaded_boot_script_header() {
        // Before any script has been loaded there is no id: a typed capability gap, never
        // a fabricated `{"Erlang/OTP", …}`.
        let mut runtime = runtime();
        let error = call(&mut runtime, "init", "script_id", &[]).expect_err("no script loaded");
        assert_eq!(
            crate::error::unsupported_reason(&error).as_deref(),
            Some("init:script_id/0 (no boot script has been loaded)")
        );

        // After loading the committed `start.boot`, the answer is its real `{Name, Vsn}`
        // header — `{"Erlang/OTP","30"}` for this release.
        let script_path =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../../bootstrap/bin/start.boot");
        let expected = crate::boot::bootscript::read(std::path::Path::new(script_path))
            .expect("the committed start.boot");
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        runtime.boot().expect("boot");
        let _ = runtime.boot_from_script_reporting(script_path);
        let answer = call(&mut runtime, "init", "script_id", &[]).expect("script_id/0");
        assert_eq!(terms::tuple_arity(answer).expect("a 2-tuple"), 2);
        assert_eq!(
            string_of(terms::tuple_element(answer, 0).unwrap()),
            expected.name
        );
        assert_eq!(
            string_of(terms::tuple_element(answer, 1).unwrap()),
            expected.vsn
        );
        assert_eq!(expected.name, "Erlang/OTP");
        assert_eq!(expected.vsn, "30");
    }

    #[test]
    fn get_argument_still_reports_an_absent_flag_as_error() {
        // A flag the runtime has no value for is the atom `error` — the automatic flags
        // do not change that rule for any other name.
        let mut runtime = runtime();
        let flag = atom(&mut runtime, "no_such_flag");
        let answer = call(&mut runtime, "init", "get_argument", &[flag]).expect("absent");
        assert_eq!(runtime.atom_name_of(answer), Some("error"));
    }
}
