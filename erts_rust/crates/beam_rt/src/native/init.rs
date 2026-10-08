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

impl RuntimeContext<'_> {
    /// `init:get_argument(Flag)`: `{ok, [[Value…]…]}` for a flag that was given (one
    /// inner list per occurrence, empty for a valueless flag), or the atom `error`.
    ///
    /// The occurrences are **snapshotted** out of the configuration before any term is
    /// built: every occurrence is owned data by then, so no borrow of the runtime is
    /// held across the mutable term construction below.
    pub(super) fn get_argument(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let flag = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
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
    /// `root`/`bindir`/`progname`/`home` entries, which this runtime does not model (see
    /// the crate `CONTEXT.md`, Known Issues) — so a `get_arguments/0` comparison against
    /// the oracle is expected to differ by exactly those four.
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
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `init:*` entries: the command-line surface
    //! (`get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`) and the boot status.

    use super::*;
    use crate::native::test_support::{atom, call, runtime, string_of};
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
}
