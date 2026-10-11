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

//! The `init:*` runtime-native floor: the parsed command line (contract A.6), the boot
//! status (contract A.7) and the node stop request.
//!
//! `init` is a **preloaded** module: the oracle serves these calls from `init.beam`, which
//! is written against the process/message machinery Milestone-1 does not run. The runtime
//! therefore answers them directly from its own state — the parsed
//! [`RuntimeConfig::init_args`](crate::config::InitArgs) for the command line, the
//! terminal boot state for `get_status/0` — instead of loading a module it cannot execute
//! (see the parent module's docs).
//!
//! # `init:stop/0,1`
//!
//! `init:stop` does **not** terminate the caller: `init.erl:537,570` sends the stop request
//! to the `init` process (`init ! {stop, …}`) and returns `ok` at once, and it is that
//! process which eventually halts the **node**. This runtime has no live `init` process, so
//! the `erlang:halt/1`-equivalent outcome is recorded on the runtime and applied by the host
//! once the running action has finished ([`crate::cli::run`]) — the caller keeps running,
//! exactly as the oracle's does. Only the argument **rejections** (`badarg`) and the
//! crash-dump slogan form are raised in the caller, as the oracle raises them.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_small, signed_value, Eterm};

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

    /// `init:get_arguments/0`: the whole command line as `{Flag, [Value…]}` pairs
    /// (contract A.6).
    ///
    /// The emulator's own **automatic** flags come first, in exactly the order `init.erl`
    /// defines them (`init.erl:363-388`): `root`, `bindir`, `progname`, `home` — each
    /// `{Flag, [Value]}` with the single string `init:get_argument/1` answers for the same
    /// flag (one occurrence, one value). An automatic flag this runtime has no value for is
    /// **omitted** from the list entirely — never reported with a fabricated value: the
    /// oracle itself omits `{home,…}` when `$HOME` is unset (verified against OTP
    /// 29/erts-17.1), and `bindir`/`progname` are omitted here under the same rule.
    ///
    /// After them come the command-line flags, from
    /// [`RuntimeConfig::init_args`](crate::config::InitArgs) — the same source as
    /// `init:get_argument/1`: a bare flag's value list is empty, and each value is a
    /// string (a char list).
    ///
    /// Oracle-verified ordering and shape (`erl -noshell -eval
    /// 'io:format("~p~n",[init:get_arguments()])'`):
    /// `[{root,["<root>"]},{bindir,["<root>/erts-17.1/bin"]},{progname,["erl"]},
    /// {home,["<home>"]},{noshell,[]}]`.
    pub(super) fn get_arguments(&mut self) -> Result<Eterm> {
        // The automatic entries — the emulator's own flags, which are not command-line
        // flags and so have no `InitArgs` entry. Every value comes from the source
        // `init:get_argument/1` already uses (see [`Self::get_argument`]).
        let mut pairs = Vec::new();
        // `root` is a hard error when the installation cannot be resolved at all, exactly
        // as it is for `init:get_argument(root)`: an unresolvable installation is a broken
        // boot, not a flag with no value.
        let root = self.code_root_dir()?;
        pairs.push(self.automatic_entry("root", root)?);
        // `bindir`/`progname`/`home` are **omitted** when this runtime has no value for
        // them (`bindir` needs an emulator directory under the resolved installation,
        // `progname` needs a UTF-8 `$PROGNAME`, `home` a UTF-8 `$HOME`).
        let _ = self.runtime.code_server.add_system_paths();
        if let Some(dir) = self.runtime.code_server.bindir() {
            let value = self.charlist(&dir.display().to_string())?;
            pairs.push(self.automatic_entry("bindir", value)?);
        }
        if let Some(name) = progname() {
            let value = self.charlist(&name)?;
            pairs.push(self.automatic_entry("progname", value)?);
        }
        if let Ok(home) = std::env::var("HOME") {
            let value = self.charlist(&home)?;
            pairs.push(self.automatic_entry("home", value)?);
        }

        // Snapshot first: every value is owned data by then, so no borrow of the
        // runtime is held across the mutable term construction below.
        let occurrences: Vec<(String, Vec<String>)> = self
            .runtime
            .init_args()
            .occurrences()
            .map(|(flag, values)| (flag.to_string(), values.to_vec()))
            .collect();

        for (flag, values) in &occurrences {
            let flag_atom = self.atom(flag)?;
            let value_list = self.string_list(values)?;
            pairs.push(terms::make_tuple(self, &[flag_atom, value_list])?);
        }
        terms::make_list(self, &pairs)
    }

    /// `{Flag, [Value]}` — one occurrence holding the single string `value`: the shape the
    /// emulator's automatic flags have in `init:get_arguments/0` (and the shape
    /// `init:get_argument/1` answers for the same flags).
    fn automatic_entry(&mut self, flag: &str, value: Eterm) -> Result<Eterm> {
        let flag_atom = self.atom(flag)?;
        let values = terms::make_list(self, &[value])?;
        terms::make_tuple(self, &[flag_atom, values])
    }

    /// `init:stop/0`: report `ok` to the caller and ask the node to terminate with status
    /// 0 (`init.erl:537` — `stop() -> init ! {stop,stop}, ok`).
    ///
    /// The caller is **not** terminated: the request goes to the `init` process, which
    /// halts the node once it gets to it. This runtime has no live `init` process
    /// (contract A.7 — the preloaded `init.beam` is not run), so the status is recorded and
    /// the host exits with it after the action has finished ([`crate::cli::run`]); the
    /// expression keeps running — oracle-verified, `io:format("a~n"), init:stop(),
    /// io:format("b~n")` prints both lines and exits 0.
    pub(super) fn stop_0(&mut self) -> Result<Eterm> {
        self.record_stop(0)
    }

    /// `init:stop/1`: report `ok` to the caller and ask the node to terminate with the
    /// given status (`init.erl:550-570`).
    ///
    /// The argument taxonomy is the oracle's, clause for clause:
    ///
    /// * a **non-negative integer** (small or bignum) is the exit status the node halts
    ///   with — the same `term_to_Uint_mask`/`code & INT_MAX` validation `erlang:halt/1`
    ///   uses (`super::halt_status_of`);
    /// * a **byte list** — a proper list whose every element is an integer in `0..=255`,
    ///   `[]` included (`init.erl`'s `is_bytelist/1`) — is the crash-dump **slogan** form,
    ///   i.e. what `erlang:halt(Slogan)` does: the slogan is printed, a dump written, exit 1;
    /// * anything else — the atom `normal` included, a negative integer, a tuple, a
    ///   non-byte list — is `erlang:error(badarg)` **raised in the caller**.
    ///
    /// The integer case records the status **without raising** (see [`Self::stop_0`]), so
    /// `init:stop(3)` still exits 3 once the expression has run — oracle-verified.
    pub(super) fn stop_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let status = *args.first().ok_or(BeamError::BadArg)?;
        if let Ok(code) = super::halt_status_of(status) {
            return self.record_stop(code);
        }
        if is_bytelist(status) {
            return Err(match super::halt_slogan(status) {
                Ok(slogan) => error::dump(&slogan),
                Err(error) => error,
            });
        }
        Err(BeamError::BadArg)
    }

    /// Records `status` as the node's exit status **without raising** and answers the atom
    /// `ok` — the non-terminating half of `init:stop/0,1` (`init.erl:537,570`: the caller
    /// gets `ok` back and it is the `init` process that halts the node).
    ///
    /// The counterpart of [`Self::record_halt`], which raises: `halt/1` terminates the
    /// running code, `init:stop` does not.
    fn record_stop(&mut self, status: i32) -> Result<Eterm> {
        self.runtime.halt = Some(status);
        self.atom("ok")
    }

    /// `init:restart/0`: reboot the node **in place** and answer `ok`
    /// (`erts/preloaded/src/init.erl:492-518,937-954`).
    ///
    /// The oracle's `restart/0` only *requests* the reboot — `restart() -> restart([])` and
    /// `restart([]) -> init ! {stop,restart}, ok` (`init.erl:494,514`) — and it is the `init`
    /// process that takes the applications down, unloads code, closes the ports and boots
    /// again (`clear_system/3` + `do_restart/3`, `init.erl:925-962`). This runtime has no live
    /// `init` process, so [`Runtime::restart`] performs the reboot **synchronously** and the
    /// atom `ok` is answered, which is exactly what the oracle's caller sees.
    ///
    /// The observable end state is the same — the node is booted again (preloaded set, default
    /// code path and the loaded boot script re-run) and still answers `init:*`/`code:*`. The
    /// mechanism differs where this runtime has nothing to tear down; the honest fidelity note
    /// is on [`Runtime::restart`].
    ///
    /// [`Runtime::restart`]: crate::Runtime::restart
    pub(super) fn restart_0(&mut self) -> Result<Eterm> {
        self.runtime.restart()?;
        self.atom("ok")
    }

    /// `init:boot/1`: the boot **entry point** (`erts/preloaded/src/init.erl:585-597`).
    ///
    /// `BootArgs` is the command line **minus the emulator flags**, a list of binaries
    /// (`-spec boot(BootArgs) -> no_return() when BootArgs :: [binary()]`). The `-boot
    /// <Script>` flag selects the boot script; with no such flag `init` boots
    /// `get_flag(boot, Flags, Root ++ "/bin/start")` (`init.erl:1257-1258`, whose value is
    /// `$ROOT/bin/start.boot`), the same default this floor uses.
    ///
    /// The value is resolved with the CLI's own rule
    /// ([`resolve_boot_file_for`](crate::cli::resolve_boot_file_for)), so `init:boot/1` and the
    /// `-boot FILE` action can never disagree, and the script is executed with
    /// [`Runtime::boot_from_script`].
    ///
    /// # Fidelity limit (honest, not hidden)
    ///
    /// The real `boot/1` **never returns**: it registers itself as `init`, traps exits and
    /// enters its receive loop (`init.erl:587-597`) — a process this runtime does not have.
    /// This floor therefore runs the boot and then answers the atom `ok`, letting the caller's
    /// expression go on (or the CLI exit normally). The boot itself is real; only the
    /// never-returning loop around it is absent.
    ///
    /// A non-byte-list argument is `badarg` (the floor's convention); a `-boot` value whose
    /// script cannot be resolved is that resolution's typed `cannot get bootfile …` error —
    /// never a silent skip, never a different script.
    pub(super) fn boot_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let boot_args = *args.first().ok_or(BeamError::BadArg)?;
        let arguments = boot_args_strings(boot_args)?;
        let value = boot_flag_value(&arguments).unwrap_or(DEFAULT_BOOT_FILE);
        // `<Root>` — which `$ROOT` and the `<Root>/bin` fallback are resolved against — comes
        // from the system `ebin` directory on the code path, so the runtime is booted before
        // the value is resolved (`boot_from_script` shares the precondition anyway).
        self.runtime.boot()?;
        let path = crate::cli::resolve_boot_file_for(&self.runtime, value)?;
        self.runtime.boot_from_script(&path)?;
        self.atom("ok")
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

/// `init.erl`'s `is_bytelist/1` (`init.erl:564-566`): a **proper** list whose every
/// element is an integer in `0..=255`. The empty list qualifies; a non-list, an improper
/// list and any other element do not.
///
/// It is the guard that separates `init:stop/1`'s two list forms: a byte list is the
/// crash-dump **slogan** (`erlang:halt(Slogan)`), while any other list is
/// `erlang:error(badarg)`.
fn is_bytelist(term: Eterm) -> bool {
    match terms::list_elements(term) {
        Ok(elements) => elements
            .iter()
            .all(|element| is_small(*element) && (0..=255).contains(&signed_value(*element))),
        Err(_) => false,
    }
}

/// The default boot script of `init:boot/1` when no `-boot` flag is given.
///
/// `bootfile/2` resolves it as `get_flag(boot, Flags, Root ++ "/bin/start")`
/// (`init.erl:1257-1258`), i.e. the file `<Root>/bin/start.boot`. Written here in the `$ROOT`
/// form [`resolve_boot_file_for`](crate::cli::resolve_boot_file_for) expands, so the default
/// and an explicit `-boot $ROOT/start` are the very same file.
const DEFAULT_BOOT_FILE: &str = "$ROOT/start";

/// The **binaries** of an `init:boot/1` argument list — `BootArgs` is `[binary()]`
/// (`init.erl:585-587`).
///
/// A non-list argument, a non-binary element and an element whose bytes are not UTF-8 (so
/// have no script-name image) are all [`BeamError::BadArg`] — the floor's convention.
fn boot_args_strings(term: Eterm) -> Result<Vec<String>> {
    terms::list_elements(term)?
        .into_iter()
        .map(|argument| {
            let bytes = terms::binary_bytes(argument)?;
            String::from_utf8(bytes.to_vec()).map_err(|_| BeamError::BadArg)
        })
        .collect()
}

/// The value of the **first** `-boot` flag in a boot-argument list, or `None` when the flag
/// is absent (or carries no value).
///
/// `init`'s `parse_boot_args/4` turns `-boot <Value>` into the `{boot, [<Value>]}` flag and
/// `get_flag(boot, …)` answers its single value (`init.erl:1700-1703,1762-1772`). A value is
/// the first following token that does not itself begin with `-` (a plain argument
/// *terminates* the flag's value list). A bare `-boot` with no value is `None` here: the
/// oracle's own `b2s(true)` on it is a crash, not a default, so this floor honestly falls back
/// to the default script rather than inventing a value.
fn boot_flag_value(arguments: &[String]) -> Option<&str> {
    let mut tokens = arguments.iter();
    while let Some(token) = tokens.next() {
        if token == "-boot" {
            return tokens
                .next()
                .filter(|value| !value.starts_with('-'))
                .map(String::as_str);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `init:*` entries: the command-line surface
    //! (`get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`), the emulator's
    //! automatic flags (`root`/`home`/`progname`) and the boot status.

    use super::*;
    use crate::native::test_support::{atom, call, chars, runtime, string_of, system_runtime};
    use crate::runtime::Runtime;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    /// The emulator's own **automatic** flags, in the order `init:get_arguments/0` lists
    /// them (`init.erl:363-388`).
    const AUTOMATIC_FLAGS: [&str; 4] = ["root", "bindir", "progname", "home"];

    /// Decodes an `init:get_arguments/0` answer into `(Flag, [Value…])` pairs, asserting
    /// the documented shape as it goes: every entry is a 2-tuple whose first element is an
    /// atom and whose second is a list of strings.
    fn decode_pairs(runtime: &mut Runtime, all: Eterm) -> Vec<(String, Vec<String>)> {
        let entries = terms::list_elements(all).expect("a list of pairs");
        entries
            .iter()
            .map(|entry| {
                assert_eq!(terms::tuple_arity(*entry).expect("a pair"), 2);
                let flag = terms::tuple_element(*entry, 0).expect("a flag atom");
                let values = terms::tuple_element(*entry, 1).expect("a value list");
                let values = terms::list_elements(values).expect("a value list");
                for value in &values {
                    assert!(
                        terms::list_elements(*value).is_ok(),
                        "every value is an Erlang string: {value:?}"
                    );
                }
                (
                    runtime
                        .atom_name_of(flag)
                        .expect("the flag is an atom")
                        .to_string(),
                    values.iter().map(|value| string_of(*value)).collect(),
                )
            })
            .collect()
    }

    /// `Runtime::new(config)` plus a **resolved** system code path, or `None` (the caller
    /// skips **loudly**) when no system Erlang is available — the automatic `root`/`bindir`
    /// entries need one.
    fn system_runtime_with(flags: &[(&str, Vec<&str>)]) -> Option<Runtime> {
        let mut config = crate::config::RuntimeConfig::default();
        for (flag, values) in flags {
            config
                .init_args
                .record(flag, values.iter().map(|value| value.to_string()).collect());
        }
        let mut runtime = Runtime::new(config).expect("a runtime with init args");
        match runtime.code_server.add_system_paths() {
            Ok(()) => Some(runtime),
            Err(error) => {
                eprintln!("SKIP: no system code path ({error})");
                None
            }
        }
    }

    #[test]
    fn get_arguments_prepends_the_automatic_flags_in_order() {
        // Oracle-verified (`erl -noshell -eval
        // 'io:format("~p~n",[init:get_arguments()])'`):
        // `[{root,["<root>"]},{bindir,["<root>/erts-17.1/bin"]},{progname,["erl"]},
        // {home,["<home>"]},{noshell,[]}]` — the emulator's four **automatic** flags come
        // first, in that order, each `{Flag, [[Value]]}` (one occurrence, one string), and
        // the command-line flags follow.
        let Some(mut runtime) = system_runtime_with(&[("noshell", vec![]), ("pa", vec!["/a"])])
        else {
            return;
        };
        let all = call(&mut runtime, "init", "get_arguments", &[]).expect("get_arguments/0");
        let pairs = decode_pairs(&mut runtime, all);
        let flags: Vec<&str> = pairs.iter().map(|(flag, _)| flag.as_str()).collect();

        // The automatic flags that have a value here, in order — a *prefix* of the
        // canonical list, and always led by `root`.
        let automatic: Vec<&str> = flags
            .iter()
            .take_while(|flag| AUTOMATIC_FLAGS.contains(flag))
            .copied()
            .collect();
        assert_eq!(
            automatic,
            &AUTOMATIC_FLAGS[..automatic.len()],
            "the automatic flags come first, in the emulator's order: {flags:?}"
        );
        // `root` and `bindir` are always available on a resolved installation.
        assert!(automatic.len() >= 2, "root and bindir: {flags:?}");
        // Each automatic entry holds exactly one value, and it is the string
        // `init:get_argument/1` answers for the same flag.
        for (index, flag) in automatic.iter().enumerate() {
            assert_eq!(pairs[index].1.len(), 1, "{flag} has one value");
            let flag_term = atom(&mut runtime, flag);
            let via_get_argument =
                call(&mut runtime, "init", "get_argument", &[flag_term]).expect("get_argument/1");
            assert_eq!(
                Some(pairs[index].1[0].clone()),
                single_automatic_value(&mut runtime, via_get_argument),
                "{flag} agrees with init:get_argument/1"
            );
        }
        // The command-line flags follow the automatic ones, as today.
        assert_eq!(
            flags[automatic.len()..].to_vec(),
            vec!["noshell", "pa"],
            "the command line follows the automatic flags: {flags:?}"
        );
        assert!(
            pairs[automatic.len()].1.is_empty(),
            "a bare flag has no value"
        );
        assert_eq!(pairs[automatic.len() + 1].1, vec!["/a".to_string()]);
    }

    #[test]
    fn get_arguments_of_a_flagless_runtime_lists_only_the_automatic_flags() {
        // `Runtime::with_defaults()` models an empty command line, so the only entries are
        // the emulator's own automatic flags — one value each, never a synthesised
        // command-line flag.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let all = call(&mut runtime, "init", "get_arguments", &[]).expect("get_arguments/0");
        let pairs = decode_pairs(&mut runtime, all);
        assert_eq!(
            pairs.first().map(|(flag, _)| flag.as_str()),
            Some("root"),
            "the automatic flags lead the list: {pairs:?}"
        );
        for (flag, values) in &pairs {
            assert!(
                AUTOMATIC_FLAGS.contains(&flag.as_str()),
                "a flagless command line has no `{flag}` entry"
            );
            assert_eq!(values.len(), 1, "{flag} has exactly one value");
        }
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
        let script_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bootstrap/bin/start.boot"
        );
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

    // -- init:stop/0,1 --------------------------------------------------------

    #[test]
    fn stop_records_the_status_without_raising() {
        // `init:stop(N)` and `init:stop()` answer the atom `ok` to the caller **and** ask
        // the node to terminate: the status is recorded, not raised, so the expression that
        // made the call keeps running (oracle-verified: `io:format("a~n"), init:stop(),
        // io:format("b~n")` prints both lines and exits 0).
        for status in [0, 3, 256] {
            let mut runtime = runtime();
            let answer = call(
                &mut runtime,
                "init",
                "stop",
                &[beam_term::make_small(status)],
            )
            .expect("init:stop/1 answers ok");
            assert_eq!(runtime.atom_name_of(answer), Some("ok"));
            assert_eq!(runtime.halt_status(), Some(status as i32));
        }
        // `init:stop/0` is `init:stop(0)`.
        let mut runtime = runtime();
        let answer = call(&mut runtime, "init", "stop", &[]).expect("init:stop/0 answers ok");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
        assert_eq!(runtime.halt_status(), Some(0));
    }

    #[test]
    fn stop_of_a_byte_list_is_the_crash_dump_slogan() {
        // `init.erl`'s `is_bytelist/1` sends the byte list to `erlang:halt(Slogan)`: the
        // slogan form, i.e. print it, write a crash dump and exit 1. `[]` qualifies too (an
        // empty slogan).
        let mut runtime = runtime();
        let slogan = chars(&mut runtime, "hello");
        let error = call(&mut runtime, "init", "stop", &[slogan]).expect_err("a dump");
        assert_eq!(error::dump_slogan(&error), Some("hello"));
        assert_eq!(
            runtime.halt_status(),
            None,
            "the slogan path records nothing"
        );

        let empty = chars(&mut runtime, "");
        assert!(terms::list_elements(empty).expect("a list").is_empty());
        let error = call(&mut runtime, "init", "stop", &[empty]).expect_err("a dump");
        assert_eq!(error::dump_slogan(&error), Some(""));
    }

    #[test]
    fn stop_rejects_every_non_status_non_bytelist_argument() {
        // The oracle's fall-through clause: anything that is neither a non-negative integer
        // nor a byte list is `erlang:error(badarg)` **raised in the caller** — the atom
        // `normal` included (`init:stop(normal)` exits 1 under the oracle; it is not a
        // "stop normally" spelling).
        let mut runtime = runtime();
        let normal = atom(&mut runtime, "normal");
        let non_byte_list = chars(&mut runtime, "€"); // U+20AC > 255
        let tuple = tuple(&mut runtime, &[normal]);
        for argument in [normal, non_byte_list, tuple, beam_term::make_small(-1)] {
            let error = call(&mut runtime, "init", "stop", &[argument]).expect_err("badarg");
            assert_eq!(error.as_atom(), Some("badarg"), "{argument:?}");
            assert_eq!(runtime.halt_status(), None, "{argument:?} records nothing");
        }
    }

    /// A tuple built on the runtime's own arena (an `init:stop/1` argument shape the oracle
    /// rejects).
    fn tuple(runtime: &mut Runtime, elems: &[Eterm]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_tuple(&mut context, elems).expect("a tuple")
    }

    // -- init:restart/0 and init:boot/1 ---------------------------------------

    /// `bytes` as an Erlang binary on a runtime heap that never moves and is never reset.
    ///
    /// Built by [`beam_codec::term::make_binary`] — the width-independent `ErlHeapBits`
    /// writer the runtime's own `prim_file:read_file/1` uses — rather than
    /// [`terms::make_heap_binary`], which refuses a byte string longer than
    /// [`ERL_ONHEAP_BINARY_LIMIT`](beam_term::refc::ERL_ONHEAP_BINARY_LIMIT) (64) bytes.
    /// The `-boot` value below **is** a `$TMPDIR` scratch path, so its length is the
    /// environment's to decide: a longer `$TMPDIR` (or a wider pid/counter in the
    /// scratch name) must not turn this test into a spurious failure.
    /// [`terms::binary_bytes`] — what `init:boot/1`'s argument reader uses — reads the
    /// term either way, so the assertions are unaffected.
    fn binary(runtime: &mut Runtime, bytes: &[u8]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        beam_codec::term::make_binary(context.eval_heap(), bytes).expect("a binary")
    }

    /// A list of `elements`, built on the runtime's pointer-stable arena.
    fn list(runtime: &mut Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_list(&mut context, elements).expect("a list")
    }

    /// The canonical ETF image of `{script, {"T","1"}, [{progress,started}]}` — a well-formed
    /// boot script (byte-for-byte what `term_to_binary/1` produces for that term) whose only
    /// directive is informational, so it always executes to the end.
    const MINIMAL_SCRIPT: &[u8] = &[
        131, 104, 3, 119, 6, b's', b'c', b'r', b'i', b'p', b't', 104, 2, 107, 0, 1, b'T', 107, 0,
        1, b'1', 108, 0, 0, 0, 1, 104, 2, 119, 8, b'p', b'r', b'o', b'g', b'r', b'e', b's', b's',
        119, 7, b's', b't', b'a', b'r', b't', b'e', b'd', 106,
    ];

    /// Writes [`MINIMAL_SCRIPT`] to a fresh `$TMPDIR/<label>/mini.boot`; returns the file path
    /// and its **suffix-less** stem (the value a `-boot` flag takes, since `init` appends
    /// `.boot` itself).
    fn write_minimal_script(label: &str) -> (PathBuf, PathBuf) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_init_{}_{}_{label}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let stem = dir.join("mini");
        let path = stem.with_extension("boot");
        std::fs::write(&path, MINIMAL_SCRIPT).expect("the fixture script");
        (path, stem)
    }

    #[test]
    fn restart_reboots_the_node_and_keeps_it_answering() {
        // `init:restart/0` answers the atom `ok` (`init.erl:494,514`: `restart() -> restart([])`
        // and `restart([]) -> init ! {stop,restart}, ok`) and the node is booted again by the
        // time the caller sees it.
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        // A directory a boot script's `{path, …}` directive (or a later `add_code_path`) would
        // have appended: the reboot resets the code path to its boot-time default.
        let added = PathBuf::from("/not/a/real/code/path");
        let boot_default = runtime.code_server().paths().to_vec();
        assert!(
            !boot_default.contains(&added),
            "the addition is not part of the default path yet"
        );
        runtime.add_code_path(added.clone());
        assert!(runtime.code_server().paths().contains(&added));

        let answer = call(&mut runtime, "init", "restart", &[]).expect("init:restart/0");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
        assert!(runtime.is_booted(), "the reboot re-boots the runtime");
        assert_eq!(
            runtime.code_server().paths(),
            &boot_default[..],
            "the code path is back to its boot-time default"
        );

        // The node still answers after the reboot — the two observables a caller can check.
        let status = call(&mut runtime, "init", "get_status", &[]).expect("get_status/0");
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(status, 0).expect("starting")),
            Some("starting")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(status, 1).expect("started")),
            Some("started")
        );
        let path = call(&mut runtime, "code", "get_path", &[]).expect("code:get_path/0");
        let entries = terms::list_elements(path).expect("a list of directories");
        assert!(!entries.is_empty(), "the code path survived the reboot");
        assert!(
            entries.iter().all(|dir| terms::list_elements(*dir).is_ok()),
            "every code-path entry is still an Erlang string"
        );
    }

    #[test]
    fn restart_re_runs_the_loaded_boot_script() {
        // The reboot re-runs the script the runtime loaded, exactly as the oracle's
        // `do_restart/3` re-runs `boot(Start, Flags, Args)` (`init.erl:951-954`).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let (path, _stem) = write_minimal_script("rerun");
        runtime.boot().expect("boot");
        runtime
            .boot_from_script(&path)
            .expect("the minimal script executes");
        assert_eq!(runtime.boot_script_id(), Some(("T", "1")));
        assert_eq!(runtime.boot_script_path(), Some(path.as_path()));

        let answer = call(&mut runtime, "init", "restart", &[]).expect("init:restart/0");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
        // The script really ran a second time: the id it established is the one the floor
        // reports, so the re-run did not drop (or replace) the loaded script.
        let id = call(&mut runtime, "init", "script_id", &[]).expect("script_id/0");
        assert_eq!(
            string_of(terms::tuple_element(id, 0).expect("the name")),
            "T"
        );
        assert_eq!(
            string_of(terms::tuple_element(id, 1).expect("the version")),
            "1"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn boot_runs_the_script_named_by_the_boot_flag() {
        // `init:boot([<<"-boot">>, <<"<stem>">>])` runs the script `init`'s own `get_boot/2`
        // would resolve (the value takes a `.boot` suffix — `init.erl:1265-1269`) and answers
        // `ok` where the real `boot/1` never returns (`init.erl:587-597`).
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let (path, stem) = write_minimal_script("boot");
        let flag = binary(&mut runtime, b"-boot");
        let value = binary(
            &mut runtime,
            stem.to_str().expect("a UTF-8 scratch path").as_bytes(),
        );
        let arguments = list(&mut runtime, &[flag, value]);

        let answer = call(&mut runtime, "init", "boot", &[arguments]).expect("init:boot/1");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
        assert_eq!(runtime.boot_script_id(), Some(("T", "1")));
        assert_eq!(runtime.boot_script_path(), Some(path.as_path()));
        // The node is booted and still answers.
        let status = call(&mut runtime, "init", "get_status", &[]).expect("get_status/0");
        assert_eq!(terms::tuple_arity(status).expect("a 2-tuple"), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn boot_rejects_a_non_byte_list_argument() {
        // `BootArgs` is `[binary()]` (`init.erl:585-587`): a non-list argument, an element that
        // is not a binary and a binary that is not UTF-8 (so has no script-name image) are all
        // the floor's `badarg`.
        let mut runtime = runtime();
        let non_list = call(&mut runtime, "init", "boot", &[beam_term::make_small(7)])
            .expect_err("a non-list argument");
        assert_eq!(non_list.as_atom(), Some("badarg"));

        let char_list = chars(&mut runtime, "-boot");
        let not_binaries = call(&mut runtime, "init", "boot", &[char_list])
            .expect_err("a list of integers is not a list of binaries");
        assert_eq!(not_binaries.as_atom(), Some("badarg"));

        let non_utf8 = binary(&mut runtime, &[0xff, 0xfe]);
        let arguments = list(&mut runtime, &[non_utf8]);
        let no_image = call(&mut runtime, "init", "boot", &[arguments])
            .expect_err("a non-UTF-8 binary has no script name");
        assert_eq!(no_image.as_atom(), Some("badarg"));
    }

    #[test]
    fn boot_flag_value_reads_the_first_boot_flag() {
        // `init`'s `parse_boot_args/4` turns `-boot <Value>` into the `{boot, [<Value>]}` flag
        // and `get_flag(boot, …)` answers its single value (`init.erl:1700-1703,1762-1772`).
        let args = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        assert_eq!(boot_flag_value(&args(&["-boot", "start"])), Some("start"));
        assert_eq!(
            boot_flag_value(&args(&["-noshell", "-boot", "first"])),
            Some("first")
        );
        // The first occurrence wins (`lists:keyfind/3`).
        assert_eq!(
            boot_flag_value(&args(&["-boot", "a", "-boot", "b"])),
            Some("a")
        );
        // A value token that is itself a flag terminates the flag's value list, and a bare
        // `-boot` has no value to report — never an invented one.
        assert_eq!(boot_flag_value(&args(&["-boot", "-noshell"])), None);
        assert_eq!(boot_flag_value(&args(&["-boot"])), None);
        assert_eq!(boot_flag_value(&args(&["-noshell"])), None);
        assert_eq!(boot_flag_value(&args(&[])), None);
        // With no `-boot` at all, `init:boot/1` boots `$ROOT/bin/start.boot`
        // (`init.erl:1257-1258`).
        assert_eq!(DEFAULT_BOOT_FILE, "$ROOT/start");
    }}
