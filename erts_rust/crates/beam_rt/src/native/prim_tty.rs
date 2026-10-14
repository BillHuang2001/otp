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
//! The runtime-native `prim_tty:*` floor (contract **A.7**, the `prim_*` family).
//!
//! ERTS serves the `prim_tty` NIF library from its **static NIF table**
//! (`erts/emulator/nifs/common/prim_tty_nif.c`, resolved by `is_static_nif_module`,
//! `erts/emulator/beam/erl_nif.c`), so `erlang:load_nif("prim_tty", …)` never `dlopen`s a
//! shared object. This module is the Rust runtime's counterpart, in two halves:
//!
//! 1. [`is_builtin_library`] — the ONE source of truth behind the resolution half (task 2.1):
//!    `crate::context::NifBridge`'s `NifLoader::load_nif` consults it before touching the
//!    filesystem, and answers `ok` for the built-in library exactly as the static-NIF-table
//!    lookup does. Nothing is inserted into the runtime's NIF registry, so
//!    `NifRegistry::is_loaded("prim_tty")` stays `false` and the module's *Erlang* wrappers
//!    (`load/0`, `init/1`, `writer/1`, `reader/1`, …) stay enterable in-machine (a
//!    `resolve_external_call` refusal would route them onto the non-pumping nested
//!    `call_bif` path, where their `receive`s fold into the typed `blocked()` gap).
//! 2. [`NIF_KEYS`] + [`RuntimeContext::prim_tty_call`] — the NIF stubs themselves, served
//!    from the floor. Both entry routes converge on the ONE implementation: the **exported**
//!    stubs (`isatty/1`, `write_nif/2`, … — reached by `call_ext`) travel
//!    `floor_shadows` → `call_bif` → `try_native`, and the **non-exported** local stubs
//!    (`tty_create/1`, `tty_init/2`, … — lowered to a LOCAL `call`) travel
//!    `ExecutionContext::resolve_local_call` → `call_bif` → `try_native`.
//!
//! # Fidelity: this runtime models a **no-`HAVE_TERMCAP`** build
//!
//! `tty_setupterm_nif`/`tty_tinfo_nif`/`tty_tigetnum_nif`/`tty_tigetflag_nif`/
//! `tty_tigetstr_nif`/`tty_tputs_nif` all return the C's own `make_enotsup` arm
//! (`{error,enotsup}`), which is what `prim_tty_nif.c` compiles to when `HAVE_TERMCAP` is
//! undefined. That is the honest answer: ncurses terminfo state is not reproducible from
//! safe Rust, and the arm is **non-raising** — mandatory, because `prim_tty:init/1` does
//! `setupterm() =:= ok` on the boot path. The C's `#else` arms return `make_enotsup`
//! *before* inspecting any argument, so neither do these.
//!
//! `isprint/1`, `wcwidth/1`, `wcswidth/1` and `tty_select/2` are typed capability gaps
//! ([`crate::error::unsupported`]): libc's locale/Unicode width tables have no faithful
//! safe-std equivalent (`libc::wcwidth` is `unsafe` and this crate is
//! `#![deny(unsafe_code)]`), and arming an async NIF read-select is not modelled. A typed
//! gap, never a fabricated width.
//!
//! # The opaque tty handle
//!
//! `tty_create/1` mints a reference and records its `ofd` in `Runtime::prim_tty_handles`
//! (keyed by the reference id). The term is the oracle's `#Ref<…>`-shaped opaque resource:
//! round-trippable, unobservable, and a term that is not in the map is the C's
//! `enif_get_resource` failure → `badarg`.
use std::io::IsTerminal;
use std::io::Write;
use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_small, signed_value, Eterm};
use beam_process::monitor::MonitorRef;
use super::RuntimeContext;

/// The `prim_tty` NIF library name — the ONE source of truth for the built-in resolution.
///
/// `prim_tty:load/0` calls `erlang:load_nif(atom_to_list(?MODULE), #{})`, so the `Path`
/// argument is this name; ERTS resolves it against its static NIF table.
pub(crate) const LIBRARY: &str = "prim_tty";

/// Every `{function, arity}` the `prim_tty` NIF library exports — the ONE source of truth
/// shared by `floor_keys()` (the exported route) and [`serves_local_call`] (the local
/// route). Mirrors `prim_tty.erl`'s `-nifs([...])` and the `ERL_NIF_INIT` table of
/// `erts/emulator/nifs/common/prim_tty_nif.c`.
pub(crate) const NIF_KEYS: [(&str, u32); 20] = [
    ("isatty", 1),
    ("tty_create", 1),
    ("tty_init", 2),
    ("setlocale", 1),
    ("tty_select", 2),
    ("tty_window_size", 1),
    ("tty_encoding", 1),
    ("tty_is_open", 2),
    ("write_nif", 2),
    ("read_nif", 3),
    ("isprint", 1),
    ("wcwidth", 1),
    ("wcswidth", 1),
    ("sizeof_wchar", 0),
    ("setupterm_nif", 0),
    ("tigetnum_nif", 1),
    ("tigetflag_nif", 1),
    ("tigetstr_nif", 1),
    ("tinfo_nif", 0),
    ("tputs_nif", 2),
];

/// Whether `name` is the built-in (statically linked) `prim_tty` NIF library — ERTS's
/// `is_static_nif_module`. The caller must skip the `dlopen` for it and answer `ok`.
///
/// Because ERTS resolves the built-in from the library path and not from the caller's
/// module identity, the caller passes **both** the `Path` argument and the calling module
/// name; either naming `prim_tty` is a hit.
pub(crate) fn is_builtin_library(name: &str) -> bool {
    name == LIBRARY
}

/// Whether `(module, function, arity)` is a `prim_tty` NIF stub — the predicate behind
/// [`beam_engine::ExecutionContext::resolve_local_call`], consulted for **every** local call
/// in every module, so it must answer `false` for everything else.
pub(crate) fn serves_local_call(module: &str, function: &str, arity: u32) -> bool {
    module == LIBRARY && NIF_KEYS.contains(&(function, arity))
}

/// The `tty_get_fd` mapping (`prim_tty_nif.c`): the three standard streams, and `None` for
/// anything else (the C's `return 0` failure).
fn tty_get_fd(name: Option<&str>) -> Option<i32> {
    match name {
        Some("stdin") => Some(0),
        Some("stdout") => Some(1),
        Some("stderr") => Some(2),
        _ => None,
    }
}

/// Whether fd `fd` is a terminal, via `std::io::IsTerminal` on the standard stream it names.
/// Handles only ever carry fd 1 (`stdout`) or 2 (`stderr`); `0` is mapped too so the helper
/// is total.
fn fd_is_terminal(fd: i32) -> bool {
    match fd {
        0 => std::io::stdin().is_terminal(),
        2 => std::io::stderr().is_terminal(),
        _ => std::io::stdout().is_terminal(),
    }
}

impl RuntimeContext<'_> {
    /// `prim_tty`'s NIF stubs, or `None` for anything outside [`NIF_KEYS`] (which keeps
    /// `try_native`'s fall-through — and with it the Erlang wrappers' in-machine entry —
    /// exactly as it was).
    pub(super) fn prim_tty_call(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        if !NIF_KEYS.contains(&(function, arity)) {
            return None;
        }
        Some(self.prim_tty_dispatch(function, arity, args))
    }

    /// The one implementation both routes reach. The caller has already checked
    /// [`NIF_KEYS`], so the trailing arm is unreachable.
    fn prim_tty_dispatch(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match (function, arity) {
            // `isatty(fd)`: the C's `tty_get_fd` then `isatty(2)`. A non-std stream atom is
            // the C's `enif_make_badarg`.
            ("isatty", 1) => {
                let Some(fd) = tty_get_fd(self.atom_name(args[0]).as_deref()) else {
                    return Err(BeamError::BadArg);
                };
                if fd_is_terminal(fd) {
                    self.atom("true")
                } else {
                    self.atom("false")
                }
            }
            // `tty_create(Fd)`: fd 0 (`stdin`) is the C's failure, as is a non-std atom; the
            // success value is the opaque handle.
            ("tty_create", 1) => {
                let Some(fd) = tty_get_fd(self.atom_name(args[0]).as_deref()) else {
                    return Err(BeamError::BadArg);
                };
                if fd == 0 {
                    return Err(BeamError::BadArg);
                }
                let handle = self.mint_prim_tty_handle(fd)?;
                self.ok_tuple(handle)
            }
            // `tty_encoding(_Handle)`: the C's Unix arm is unconditionally `utf8` after the
            // resource check.
            ("tty_encoding", 1) => {
                self.prim_tty_fd(args[0])?;
                self.atom("utf8")
            }
            // `setlocale(_Handle)`: the C file compiles `PRIMITIVE_UTF8_CHECK` = 1, whose arm
            // does not inspect its argument at all and answers `primitive`.
            ("setlocale", 1) => self.atom("primitive"),
            ("setupterm_nif", 0) | ("tinfo_nif", 0) => self.error_tuple("enotsup"),
            ("tigetnum_nif", 1) | ("tigetflag_nif", 1) | ("tigetstr_nif", 1) | ("tputs_nif", 2) => {
                self.error_tuple("enotsup")
            }
            // `tty_init(Handle, Options)`: argc/map check, then the resource, then the
            // `input` key (missing ⇒ badarg). `signals` defaults to `true` and is unused
            // here. A non-terminal ofd answers the C's `unavailable` arm.
            ("tty_init", 2) => {
                let Some(options) = args.get(1).copied() else {
                    return Err(BeamError::BadArg);
                };
                let input = match self.prim_tty_map_key(options, "input")? {
                    Some(input) => input,
                    None => return Err(BeamError::BadArg),
                };
                let raw = self.atom_name(input).as_deref() == Some("raw");
                let ofd = self.prim_tty_fd(args[0])?;
                if !fd_is_terminal(ofd) {
                    return if raw {
                        self.error_tuple("enotsup")
                    } else {
                        self.ok_atom()
                    };
                }
                if raw {
                    return Err(crate::error::unsupported(
                        "prim_tty termios raw mode (no safe-std termios interface)",
                    ));
                }
                self.ok_atom()
            }
            // `tty_window_size(Handle)`: `TIOCGWINSZ` is unavailable in safe std. A
            // non-terminal ofd is the C's `{error,enotsup}`; a real terminal is the typed
            // gap rather than a fabricated geometry.
            ("tty_window_size", 1) => {
                let ofd = self.prim_tty_fd(args[0])?;
                if fd_is_terminal(ofd) {
                    return Err(crate::error::unsupported(
                        "prim_tty window size (TIOCGWINSZ ioctl)",
                    ));
                }
                self.error_tuple("enotsup")
            }
            // `write_nif(Handle, Iovec)`: flatten the `erlang:iolist_to_iovec/1` output and
            // write it to the handle's ofd.
            ("write_nif", 2) => {
                let ofd = self.prim_tty_fd(args[0])?;
                let mut bytes = Vec::new();
                if !self.prim_tty_flatten(args[1], &mut bytes) {
                    return Err(BeamError::BadArg);
                }
                let mut out: Box<dyn Write> = if ofd == 2 {
                    Box::new(std::io::stderr())
                } else {
                    Box::new(std::io::stdout())
                };
                match out.write_all(&bytes).and_then(|()| out.flush()) {
                    Ok(()) => self.ok_atom(),
                    Err(error) => self.error_tuple_for(&error),
                }
            }
            // `read_nif(Handle, Fd, N)`: the C's Unix arm reads from `tty->ifd` (hardcoded
            // fd 0) and caps the request at 1024 bytes; a 0-byte read is `{error,closed}`.
            ("read_nif", 3) => {
                self.prim_tty_fd(args[0])?;
                let count = match args.get(2).copied() {
                    Some(term) if is_small(term) => signed_value(term),
                    _ => return Err(BeamError::BadArg),
                };
                if count < 0 {
                    return Err(BeamError::BadArg);
                }
                let want = usize::try_from(u64::try_from(count).unwrap_or(u64::MAX).min(1024))
                    .unwrap_or(1024);
                let mut buffer = vec![0u8; want];
                let read = std::io::Read::read(&mut std::io::stdin(), &mut buffer);
                match read {
                    Ok(0) => self.error_tuple("closed"),
                    Ok(read) => {
                        buffer.truncate(read);
                        let binary = terms::make_heap_binary(self, &buffer)?;
                        self.ok_tuple(binary)
                    }
                    Err(error) => self.error_tuple_for(&error),
                }
            }
            // `tty_is_open(Handle, Fd)`: a liveness probe of the fd. On Linux a closed fd has
            // no `/proc/self/fd/N` entry; elsewhere the probe is unavailable and the fd is
            // reported open (the honest approximation of the C's `poll(POLLHUP, 0)`).
            ("tty_is_open", 2) => {
                self.prim_tty_fd(args[0])?;
                let Some(fd) = tty_get_fd(self.atom_name(args[1]).as_deref()) else {
                    return Err(BeamError::BadArg);
                };
                if cfg!(target_os = "linux")
                    && std::fs::read_link(format!("/proc/self/fd/{fd}"))
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    return self.atom("false");
                }
                self.atom("true")
            }
            ("sizeof_wchar", 0) => {
                terms::make_integer(self, core::mem::size_of::<i32>() as i64)
            }
            ("isprint", 1) | ("wcwidth", 1) | ("wcswidth", 1) => Err(crate::error::unsupported(
                "prim_tty character metrics (libc locale/Unicode width tables)",
            )),
            ("tty_select", 2) => Err(crate::error::unsupported(
                "prim_tty async read select (enif_select)",
            )),
            _ => Err(BeamError::BadArg),
        }
    }

    /// Mint the opaque tty handle for `ofd`: a fresh reference from the runtime's own id
    /// space, recorded in [`Runtime::prim_tty_handles_mut`](crate::Runtime::prim_tty_handles_mut).
    fn mint_prim_tty_handle(&mut self, ofd: i32) -> Result<Eterm> {
        let reference = self.runtime.refs_mut().next();
        let mut heap = self.runtime.take_bif_heap()?;
        let term = reference.to_eterm(&mut heap)?;
        self.runtime.put_bif_heap(heap);
        self.runtime.prim_tty_handles_mut().insert(reference.id(), ofd);
        Ok(term)
    }

    /// The `ofd` of the handle `term`, or `badarg` — the C's `enif_get_resource` failure.
    fn prim_tty_fd(&mut self, term: Eterm) -> Result<i32> {
        let id = MonitorRef::from_eterm(term).map_err(|_| BeamError::BadArg)?.id();
        self.runtime
            .prim_tty_handles_mut()
            .get(&id)
            .copied()
            .ok_or(BeamError::BadArg)
    }

    /// The `key` entry of the term map `map`, or `None` when the key is absent. A term that
    /// is not a map is the caller's `badarg`.
    fn prim_tty_map_key(&mut self, map: Eterm, key: &str) -> Result<Option<Eterm>> {
        let key = self.atom(key)?;
        let atoms = self.runtime.code_server.atoms();
        match beam_term::map::map_get(map, key, atoms) {
            Ok(found) => Ok(found),
            Err(_) => Err(BeamError::BadArg),
        }
    }

    /// Flatten `term` as an Erlang iovec (`erlang:iolist_to_iovec/1` output) into `out`:
    /// a list of binaries, byte-sized integers and nested iolists. `false` for anything
    /// else — the C's `badarg` on a malformed iovec.
    fn prim_tty_flatten(&self, term: Eterm, out: &mut Vec<u8>) -> bool {
        let Ok(elements) = terms::list_elements(term) else {
            return false;
        };
        for element in elements {
            if let Ok(bytes) = terms::binary_bytes(element) {
                out.extend_from_slice(&bytes);
                continue;
            }
            if is_small(element) {
                let byte = signed_value(element);
                if !(0..=255).contains(&byte) {
                    return false;
                }
                out.push(byte as u8);
                continue;
            }
            if !self.prim_tty_flatten(element, out) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{make_small, Eterm};
    use crate::native::test_support::{atom, call, runtime};
    use crate::runtime::Runtime;

    /// The atom name of `term` (the tests assert names, never indices).
    fn name_of(runtime: &Runtime, term: Eterm) -> String {
        runtime
            .atom_name_of(term)
            .unwrap_or_else(|| panic!("{term:?} is not an atom"))
            .to_string()
    }

    /// Asserts `prim_tty:function(args)` answers the bare atom `ok`-family atom `expected`.
    fn assert_atom(runtime: &mut Runtime, function: &str, args: &[Eterm], expected: &str) {
        let answer = call(runtime, "prim_tty", function, args).expect("a served answer");
        assert_eq!(name_of(runtime, answer), expected, "{function}");
    }

    /// The reason atom of an `{error, Reason}` answer.
    fn error_reason(runtime: &Runtime, answer: Eterm) -> String {
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        let reason = terms::tuple_element(answer, 1).expect("an element");
        name_of(runtime, reason)
    }

    #[test]
    fn is_builtin_library_names_exactly_prim_tty() {
        assert!(is_builtin_library("prim_tty"));
        assert!(!is_builtin_library("prim_file"));
        assert!(!is_builtin_library(""));
        assert!(!is_builtin_library("prim_tty.so"));
    }

    #[test]
    fn serves_local_call_is_exactly_the_prim_tty_nif_keys() {
        for (function, arity) in NIF_KEYS {
            assert!(serves_local_call("prim_tty", function, arity));
            assert!(!serves_local_call("other", function, arity));
        }
        // The Erlang wrappers are NOT NIF keys, so their bodies stay enterable in-machine.
        assert!(!serves_local_call("prim_tty", "load", 0));
        assert!(!serves_local_call("prim_tty", "init", 1));
        assert!(!serves_local_call("prim_tty", "isatty", 2));
        assert!(!serves_local_call("prim_file", "read_file_info", 2));
    }

    #[test]
    fn an_unlisted_key_is_not_served() {
        let mut runtime = runtime();
        // The floor owns exactly `NIF_KEYS`; every other `prim_tty:*` is an ordinary
        // `lib/kernel/src/prim_tty.erl` body (`load/0`, `init/1`, `writer/1`, …), so
        // `try_native` answers `None` for it (it then falls through to the real module),
        // the local-call seam declines it, and `floor_shadows` lets it enter in-machine.
        for (function, arity) in [("load", 0u32), ("init", 1), ("isatty", 2), ("handles", 1)] {
            assert!(
                !serves_local_call("prim_tty", function, arity),
                "`prim_tty:{function}/{arity}` is not a NIF key"
            );
            assert!(
                !crate::native::floor_shadows("prim_tty", function, arity),
                "`prim_tty:{function}/{arity}` must stay enterable in-machine"
            );
            let mut sink = beam_bifs::NullSink;
            let mut context =
                crate::context::RuntimeContext::with_output(&mut runtime, &mut sink);
            assert!(
                context.try_native("prim_tty", function, arity, &[]).is_none(),
                "the floor must not claim `prim_tty:{function}/{arity}`"
            );
        }
    }

    #[test]
    fn isatty_is_false_for_every_standard_stream_in_a_non_tty() {
        if std::io::stdout().is_terminal() {
            return;
        }
        let mut runtime = runtime();
        for stream in ["stdin", "stdout", "stderr"] {
            let fd = atom(&mut runtime, stream);
            assert_atom(&mut runtime, "isatty", &[fd], "false");
        }
    }

    #[test]
    fn isatty_rejects_a_non_standard_stream_atom() {
        let mut runtime = runtime();
        let bad = atom(&mut runtime, "nope");
        match call(&mut runtime, "prim_tty", "isatty", &[bad]) {
            Err(BeamError::BadArg) => {}
            other => panic!("expected badarg, got {other:?}"),
        }
    }

    #[test]
    fn sizeof_wchar_is_four() {
        let mut runtime = runtime();
        let value = call(&mut runtime, "prim_tty", "sizeof_wchar", &[]).expect("a served answer");
        assert_eq!(signed_value(value), 4);
    }

    #[test]
    fn setlocale_is_primitive_without_inspecting_its_argument() {
        let mut runtime = runtime();
        assert_atom(&mut runtime, "setlocale", &[make_small(0)], "primitive");
    }

    #[test]
    fn tty_create_rejects_stdin_and_the_termcap_family_is_enotsup() {
        let mut runtime = runtime();
        let stdin = atom(&mut runtime, "stdin");
        match call(&mut runtime, "prim_tty", "tty_create", &[stdin]) {
            Err(BeamError::BadArg) => {}
            other => panic!("tty_create(stdin) must be badarg, got {other:?}"),
        }
        let answer = call(&mut runtime, "prim_tty", "setupterm_nif", &[]).expect("a served answer");
        assert_eq!(error_reason(&runtime, answer), "enotsup", "no-termcap model");
    }

    #[test]
    fn a_created_handle_round_trips_and_answers_utf8() {
        let mut runtime = runtime();
        let stderr = atom(&mut runtime, "stderr");
        let answer = call(&mut runtime, "prim_tty", "tty_create", &[stderr]).expect("a served answer");
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        let handle = terms::tuple_element(answer, 1).expect("an element");
        assert_atom(&mut runtime, "tty_encoding", &[handle], "utf8");
        let size =
            call(&mut runtime, "prim_tty", "tty_window_size", &[handle]).expect("a served answer");
        assert_eq!(error_reason(&runtime, size), "enotsup");
        // A term that is not a live handle is `badarg` — the C's `enif_get_resource` miss.
        let bogus = atom(&mut runtime, "nope");
        match call(&mut runtime, "prim_tty", "tty_encoding", &[bogus]) {
            Err(BeamError::BadArg) => {}
            other => panic!("a non-handle must be badarg, got {other:?}"),
        }
    }

    #[test]
    fn tty_init_requires_a_map_with_the_input_key() {
        let mut runtime = runtime();
        let stderr = atom(&mut runtime, "stderr");
        let answer = call(&mut runtime, "prim_tty", "tty_create", &[stderr]).expect("a served answer");
        let handle = terms::tuple_element(answer, 1).expect("an element");
        match call(&mut runtime, "prim_tty", "tty_init", &[handle, make_small(0)]) {
            Err(BeamError::BadArg) => {}
            other => panic!("a non-map must be badarg, got {other:?}"),
        }
    }

    #[test]
    fn character_metrics_and_select_are_typed_capability_gaps() {
        let mut runtime = runtime();
        for (function, arity) in [("isprint", 1u32), ("wcwidth", 1), ("wcswidth", 1), ("tty_select", 2)] {
            let args = vec![make_small(0); arity as usize];
            match call(&mut runtime, "prim_tty", function, &args) {
                Err(error) => assert!(
                    format!("{error:?}").contains("unsupported"),
                    "{function} must be an `unsupported` gap, got {error:?}"
                ),
                Ok(term) => panic!("{function} must not fabricate a value: {term:?}"),
            }
        }
    }
}
