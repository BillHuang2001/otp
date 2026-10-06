//! The runtime-native BIF floor: the BIFs `beam_bifs` deliberately does not provide.
//!
//! `beam_bifs` owns the `bif.tab` surface — arithmetic, comparison, type tests, term
//! ops, `lists`/`maps`/`binary`, `io:format`, `ets`, … — but a handful of "BIFs" are
//! really *runtime* operations that need the host's own state and are therefore served
//! here, **before** the registry is consulted:
//!
//! | Call | Why it is runtime-native |
//! |---|---|
//! | `erlang:halt/0,1` | terminates the runtime: it must travel out of the running code as a raise and become the process exit status |
//! | `erlang:display/1` | writes to the *process's* output sink |
//! | `io:put_chars/1,2` | writes to the process's output sink (the device argument is accepted and ignored) |
//! | `erlang:apply/2,3` | re-enters the host's own dispatch (BIF **or** load-on-demand module call) |
//! | `erlang:system_info/1` | reads/renders the runtime introspection surface (contract A.8) on the runtime's own scratch heap |
//! | `init:get_argument/1` | reads the parsed command line (contract A.6, `RuntimeConfig::init_args`) |
//! | `init:get_plain_arguments/0` | the (always empty) plain-argument list — the CLI models no plain arguments |
//! | `init:get_arguments/0` | the whole command line, in order, from `RuntimeConfig::init_args` |
//! | `os:cmd/1` | runs a command through `/bin/sh` and returns its stdout — the host's own process spawn |
//! | `code:get_path/0`, `code:all_loaded/0`, `code:which/1`, `code:is_loaded/1`, `code:is_sticky/1,2`, `code:ensure_loaded/1`, `code:lib_dir/0,1`, `code:root_dir/0` | read the runtime's own [`CodeServer`](crate::CodeServer) state (the code path, the loaded/preloaded module set, the installation) |
//! | `application:get_env/1,2,3`, `application:get_all_env/1`, `application:get_key/2`, `application:loaded_applications/0`, `application:which_applications/0` | read the runtime's own **application-config view** — the `.app` resource files of the OTP applications its code path resolves |
//!
//! The `init`/`code`/`application` calls are **preloaded-module surface** (contract
//! A.7): the oracle serves them from `init.beam`/`code.beam`/`application.beam`, but
//! those modules are written against the process/message machinery Milestone-1 does not
//! run (for `application`, that is the `application_controller`/`application_master`
//! process pair plus a group leader), so this runtime answers them directly from its own
//! state instead of loading a module it cannot execute. `os:cmd/1` is the same story for
//! the spawn **driver** (contract A.4): the port machinery is a later stage, but the host
//! is perfectly able to spawn a shell.
//!
//! Every entry returns `Option<Result<Eterm>>`: `None` means "not a native BIF, fall
//! through to the `beam_bifs` registry", `Some(result)` is the answer (which may be an
//! error). Nothing is re-implemented that the registry already owns, and terms are
//! built through [`beam_engine::terms`] (`beam_rt` stays `unsafe`-free) on the heap the
//! engine's `alloc_words` seam exposes.
//!
//! # The `halt` convention
//!
//! `halt` does not return a value: it raises an `exit`-class exception whose reason atom
//! is `beam_rt_halt:<status>` (see [`crate::error::halt`]). The runtime also records the
//! status ([`Runtime::halt_status`](crate::Runtime::halt_status)) so a host that catches
//! the raise can still observe it, and the CLI turns it into the process exit code.
//!
//! # Module layout
//!
//! The floor is split by responsibility: the `erlang`-core items and the shared helpers live
//! in this module; the `init:*` entries in [`init`], the `code:*` entries in [`code`], the
//! `erlang:*` process/registry/reference entries in [`process`] and the `application:*`
//! entries in [`application`]. The submodules reach the shared helpers through `super`
//! (they are `pub(super)`).

mod application;
mod code;
mod init;
mod process;
#[cfg(test)]
mod test_support;

use std::process::Command;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_term::{atom_val, is_atom, is_small, make_atom, make_small, signed_value, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::error;
use crate::format;

impl RuntimeContext<'_> {
    /// The runtime-native BIF floor, consulted **before** the `beam_bifs` registry.
    ///
    /// Returns `None` when `(module, function, arity)` is not a native BIF — the
    /// caller then dispatches through the registry (or loads the module).
    pub(crate) fn try_native(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (module, function, arity) {
            ("erlang", "halt", 0) => Some(Err(self.record_halt(0))),
            ("erlang", "halt", 1) => Some(self.halt_1(args)),
            ("erlang", "display", 1) => Some(self.display(args)),
            ("io", "put_chars", 1) => Some(self.put_chars(args, 1)),
            ("io", "put_chars", 2) => Some(self.put_chars(args, 2)),
            ("erlang", "apply", 2) => Some(self.apply_dynamic(args, false)),
            ("erlang", "apply", 3) => Some(self.apply_dynamic(args, true)),
            ("erlang", "system_info", 1) => Some(self.system_info_1(args)),
            ("init", "get_argument", 1) => Some(self.get_argument(args)),
            ("init", "get_plain_arguments", 0) => Some(self.get_plain_arguments()),
            ("init", "get_arguments", 0) => Some(self.get_arguments()),
            ("init", "get_status", 0) => Some(self.get_status()),
            ("os", "cmd", 1) => Some(self.os_cmd(args)),
            ("code", "get_path", 0) => Some(self.code_get_path()),
            ("code", "all_loaded", 0) => Some(self.code_all_loaded()),
            ("code", "which", 1) => Some(self.code_which(args)),
            ("code", "is_loaded", 1) => Some(self.code_is_loaded(args)),
            ("code", "is_sticky", 1) => Some(self.code_is_sticky_1(args)),
            ("code", "is_sticky", 2) => Some(self.code_is_sticky_2()),
            ("code", "ensure_loaded", 1) => Some(self.code_ensure_loaded(args)),
            ("code", "lib_dir", 0) => Some(self.code_lib_dir_0()),
            ("code", "lib_dir", 1) => Some(self.code_lib_dir_1(args)),
            ("code", "root_dir", 0) => Some(self.code_root_dir()),
            ("application", "get_env", 1) => Some(self.application_get_env_1()),
            ("application", "get_env", 2) => Some(self.application_get_env_2(args)),
            ("application", "get_env", 3) => Some(self.application_get_env_3(args)),
            ("application", "get_all_env", 1) => Some(self.application_get_all_env_1(args)),
            ("application", "get_key", 2) => Some(self.application_get_key_2(args)),
            ("application", "loaded_applications", 0) => {
                Some(self.application_loaded_applications())
            }
            ("application", "which_applications", 0) => {
                Some(self.application_which_applications())
            }
            ("erlang", "make_ref", 0) => Some(self.make_ref_0()),
            ("erlang", "monitor", 2) => Some(self.monitor_2(args)),
            ("erlang", "demonitor", 1) => Some(self.demonitor_1(args)),
            ("erlang", "demonitor", 2) => Some(self.demonitor_2(args)),
            ("erlang", "ports", 0) => Some(self.ports_0()),
            ("erlang", "processes", 0) => Some(self.processes_0()),
            ("erlang", "pre_loaded", 0) => Some(self.pre_loaded_0()),
            ("erlang", "group_leader", 0) => Some(self.group_leader_0()),
            ("erlang", "whereis", 1) => Some(self.whereis_1(args)),
            ("erlang", "register", 2) => Some(self.register_2(args)),
            ("erlang", "unregister", 1) => Some(self.unregister_1(args)),
            ("erlang", "module_loaded", 1) => Some(self.module_loaded_1(args)),
            _ => None,
        }
    }

    // -- erlang:halt/0,1 ------------------------------------------------------

    /// `erlang:halt/1`: terminate with the given status.
    ///
    /// A small-integer argument is the status; a non-integer (an atom such as
    /// `abort`) is reported as an abnormal termination with status `1`, exactly the
    /// observable outcome a host can act on.
    fn halt_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let status = match args.first().copied() {
            Some(argument) if is_small(argument) => {
                let status = signed_value(argument);
                let status = i32::try_from(status).map_err(|_| BeamError::BadArg)?;
                status
            }
            Some(argument) if is_atom(argument) => 1,
            _ => return Err(BeamError::BadArg),
        };
        Err(self.record_halt(status))
    }

    /// Records `status` on the runtime and returns the matching halt raise.
    fn record_halt(&mut self, status: i32) -> BeamError {
        self.runtime.halt = Some(status);
        error::halt(status)
    }

    // -- erlang:display/1 and io:put_chars/1,2 --------------------------------

    /// `erlang:display/1`: print the term on the process's output (`~p` plus a
    /// newline, as the oracle does) and return `true`.
    fn display(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let text = self.render(term, b"~p~n")?;
        self.write_output(&text)?;
        let true_atom = self.atom("true")?;
        Ok(true_atom)
    }

    /// `io:put_chars(Chars)` / `io:put_chars(Device, Chars)`: write the characters and
    /// return `ok`.
    ///
    /// The device argument of the `/2` form is accepted and ignored (Milestone-1 has
    /// one output stream).
    fn put_chars(&mut self, args: &[Eterm], which: usize) -> Result<Eterm> {
        let chars = match which {
            1 => *args.first().ok_or(BeamError::BadArg)?,
            2 => *args.get(1).ok_or(BeamError::BadArg)?,
            _ => return Err(BeamError::BadArg),
        };
        let text = self.chars_to_string(chars)?;
        self.write_output(&text)?;
        self.atom("ok")
    }

    // -- erlang:apply/2,3 -----------------------------------------------------

    /// `erlang:apply(Mod, Fun)` / `erlang:apply(Mod, Fun, Args)`.
    ///
    /// Re-enters the host's dispatch with `self` as the context, so the target may be a
    /// native BIF, a `beam_bifs` BIF **or** an external module function loaded on
    /// demand from the code path.
    fn apply_dynamic(&mut self, args: &[Eterm], with_args: bool) -> Result<Eterm> {
        let module = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        let function = self
            .atom_name(*args.get(1).ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        let call_args: Vec<Eterm> = if with_args {
            terms::list_elements(*args.get(2).ok_or(BeamError::BadArg)?)?
        } else {
            Vec::new()
        };
        let arity = call_args.len() as u32;
        self.call_bif(&module, &function, arity, &call_args)
    }

    // -- erlang:system_info/1 -------------------------------------------------

    /// `erlang:system_info(Key)`: materialize the runtime introspection key (contract
    /// A.8) on the runtime's scratch info heap.
    ///
    /// The key is read as an atom from the shared atom table; the value comes from
    /// [`Runtime::system_info`](crate::Runtime::system_info), the single source of
    /// truth for the supported keys. An unknown key yields
    /// [`BeamError::BadArg`] — the bare `badarg` the oracle raises — so `-eval`'s
    /// `catch`/`try` sees exactly what loaded code would.
    fn system_info_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let key = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        self.runtime.system_info(&key)
    }

    // -- os:cmd/1 -------------------------------------------------------------

    /// `os:cmd(Cmd)`: run `Cmd` through `/bin/sh -c` and return its **stdout**.
    ///
    /// `Cmd` is an Erlang string (a char list of bytes) and the result is an Erlang
    /// string (a list of byte small-ints), the trailing newline included. A non-zero
    /// exit status is **not** an error: whatever the command wrote to stdout is returned,
    /// exactly as the oracle does (`os:cmd/1` never consults the status). Only a failure
    /// to spawn `/bin/sh` is raised — the one thing that cannot be recovered from.
    ///
    /// The oracle reaches the same result through the port/spawn **driver** (contract
    /// A.4); Milestone-1 has no port machinery, so the host spawns the process itself.
    fn os_cmd(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let command_term = *args.first().ok_or(BeamError::BadArg)?;
        // `os:cmd/1` takes a *string* (or an iodata binary); unlike `io:put_chars/1` a
        // bare integer is not a character here — the oracle raises `badarg`.
        let is_string =
            terms::list_elements(command_term).is_ok() || terms::binary_bytes(command_term).is_ok();
        if !is_string {
            return Err(BeamError::BadArg);
        }
        let command = self.chars_to_string(command_term)?;
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .output()
            .map_err(|source| {
                error::message(format!("os:cmd/1 cannot run /bin/sh: {source}"))
            })?;
        let bytes: Vec<Eterm> = output
            .stdout
            .iter()
            .map(|byte| make_small(i64::from(*byte)))
            .collect();
        terms::make_list(self, &bytes)
    }

    // -- helpers --------------------------------------------------------------

    /// `strings` as an Erlang list of strings (each a char list).
    pub(super) fn string_list(&mut self, strings: &[String]) -> Result<Eterm> {
        let mut elems = Vec::with_capacity(strings.len());
        for text in strings {
            elems.push(self.charlist(text)?);
        }
        terms::make_list(self, &elems)
    }
    /// The name of the atom `term`, resolved through the runtime's shared atom table.
    pub(super) fn atom_name(&self, term: Eterm) -> Option<String> {
        if !is_atom(term) {
            return None;
        }
        self.runtime
            .code_server
            .atoms()
            .name(atom_val(term))
            .map(str::to_string)
    }

    /// The atom `name`, interned in the runtime's shared atom table.
    pub(super) fn atom(&mut self, name: &str) -> Result<Eterm> {
        let atom = self.runtime.code_server.atoms_mut().intern(name);
        Ok(make_atom(atom))
    }

    /// `text` as an Erlang string (a char list), built on the engine's heap seam.
    pub(super) fn charlist(&mut self, text: &str) -> Result<Eterm> {
        let elems: Vec<Eterm> = text.chars().map(|c| make_small(i64::from(c as u32))).collect();
        terms::make_list(self, &elems)
    }

    /// A term rendered by the registered `io:format` printer, using `format`'s control.
    ///
    /// The scratch heap is reset first, so the text — not any term — is the result.
    fn render(&mut self, term: Eterm, control: &[u8]) -> Result<String> {
        self.runtime.info_heap.reset();
        let runtime = &mut self.runtime;
        format::term_to_string_with(
            &runtime.registry,
            &mut runtime.info_heap,
            runtime.code_server.atoms(),
            runtime.self_pid,
            term,
            control,
        )
    }
    /// Writes `text` to the installed output sink (stdout when none is installed).
    fn write_output(&mut self, text: &str) -> Result<()> {
        use beam_bifs::OutputSink;
        match self.output.as_mut() {
            Some(sink) => sink.write_str(text),
            None => {
                let mut sink = beam_bifs::StdoutSink;
                sink.write_str(text)
            }
        }
    }

    /// Flattens an iodata term (a char list, a nested list, or a binary) to a `String`.
    fn chars_to_string(&self, term: Eterm) -> Result<String> {
        let mut out = String::new();
        self.collect_chars(term, &mut out)?;
        Ok(out)
    }

    /// Appends the characters of `term` to `out` (the [`Self::chars_to_string`] walk).
    fn collect_chars(&self, term: Eterm, out: &mut String) -> Result<()> {
        if is_small(term) {
            let code = signed_value(term);
            let code = u32::try_from(code).map_err(|_| BeamError::BadArg)?;
            let character = char::from_u32(code).ok_or(BeamError::BadArg)?;
            out.push(character);
            return Ok(());
        }
        if let Ok(bytes) = terms::binary_bytes(term) {
            out.push_str(&String::from_utf8_lossy(bytes));
            return Ok(());
        }
        if let Ok(elements) = terms::list_elements(term) {
            for element in elements {
                self.collect_chars(element, out)?;
            }
            return Ok(());
        }
        if term == NIL {
            return Ok(());
        }
        Err(BeamError::BadArg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::test_support::{atom, call, chars, runtime, string_of};

    #[test]
    fn halt_zero_raises_and_records_the_status() {
        let mut runtime = runtime();
        let error = call(&mut runtime, "erlang", "halt", &[]).expect_err("halt/0 terminates");
        assert_eq!(error::halt_status(&error), Some(0));
        assert_eq!(runtime.halt_status(), Some(0));
        assert_eq!(error::unsupported_reason(&error), None);
    }

    #[test]
    fn halt_one_carries_the_status() {
        let mut runtime = runtime();
        let error = call(&mut runtime, "erlang", "halt", &[make_small(7)])
            .expect_err("halt/1 terminates");
        assert_eq!(error::halt_status(&error), Some(7));
        assert_eq!(runtime.halt_status(), Some(7));
        // A non-numeric status is an abnormal termination (status 1), never a value.
        let error = call(&mut runtime, "erlang", "halt", &[make_small(1)]).expect_err("terminates");
        assert_eq!(error::halt_status(&error), Some(1));
    }

    #[test]
    fn display_prints_through_the_output_sink() {
        let mut runtime = runtime();
        let mut sink = beam_bifs::BytesSink::new();
        let displayed = {
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            context
                .call_bif("erlang", "display", 1, &[make_small(42)])
                .expect("display/1")
        };
        assert_eq!(runtime.atom_name_of(displayed), Some("true"));
        assert_eq!(sink.as_str(), "42\n");
    }

    #[test]
    fn put_chars_writes_char_lists_and_binaries() {
        let mut runtime = runtime();
        let mut sink = beam_bifs::BytesSink::new();
        let text = {
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            let chars = terms::make_list(&mut context, &[make_small(104), make_small(105)])
                .expect("a char list");
            context
                .call_bif("io", "put_chars", 1, &[chars])
                .expect("put_chars/1")
        };
        assert_eq!(runtime.atom_name_of(text), Some("ok"));
        assert_eq!(sink.as_str(), "hi");
        // The `/2` form takes the device first and ignores it.
        let mut sink = beam_bifs::BytesSink::new();
        {
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            // Milestone-1 has one output stream, so the device is accepted and ignored:
            // a plain small integer stands in for a device handle.
            let device = make_small(1);
            let chars =
                terms::make_list(&mut context, &[make_small(121)]).expect("a char list");
            context
                .call_bif("io", "put_chars", 2, &[device, chars])
                .expect("put_chars/2");
        }
        assert_eq!(sink.as_str(), "y");
    }

    #[test]
    fn apply_of_two_arguments_calls_a_zero_arity_bif() {
        let mut runtime = runtime();
        // `erlang:apply(erlang, self)` — a registered `erlang` BIF reached dynamically.
        let module = atom(&mut runtime, "erlang");
        let function = atom(&mut runtime, "self");
        let self_pid = call(&mut runtime, "erlang", "apply", &[module, function])
            .expect("apply/2 dispatches to the BIF registry");
        assert!(!is_small(self_pid));
        assert!(!is_atom(self_pid));
    }

    #[test]
    fn apply_of_three_arguments_unpacks_the_argument_list() {
        let mut runtime = runtime();
        let module = atom(&mut runtime, "erlang");
        let function = atom(&mut runtime, "+");
        let arguments = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[make_small(2), make_small(3)]).expect("a list")
        };
        let sum = call(&mut runtime, "erlang", "apply", &[module, function, arguments])
            .expect("apply/3");
        assert_eq!(signed_value(sum), 5);
    }

    #[test]
    fn apply_rejects_a_non_atom_target() {
        let mut runtime = runtime();
        let function = atom(&mut runtime, "self");
        assert!(matches!(
            call(&mut runtime, "erlang", "apply", &[make_small(1), function]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn a_non_native_call_falls_through() {
        // The floor must not claim `beam_bifs` surface: `+` stays the registry's.
        let mut runtime = runtime();
        let sum = call(&mut runtime, "erlang", "+", &[make_small(2), make_small(3)]).expect("+");
        assert_eq!(signed_value(sum), 5);
        // A native-shaped call the floor does not own (here: the wrong arity) falls
        // through to the registry and fails there — never silently accepted.
        let error = call(&mut runtime, "erlang", "display", &[make_small(1), make_small(2)])
            .expect_err("display/2 is no BIF");
        assert!(error.to_string().contains("erlang:display/2"), "{error}");
    }

    // -- os:cmd/1 -------------------------------------------------------------

    #[test]
    fn os_cmd_returns_stdout_as_a_byte_string() {
        let mut runtime = runtime();
        let command = chars(&mut runtime, "echo hi");
        let out = call(&mut runtime, "os", "cmd", &[command]).expect("os:cmd/1");
        // `echo hi` writes "hi\n" — three bytes, the newline included.
        assert_eq!(string_of(out), "hi\n");
    }

    #[test]
    fn os_cmd_does_not_treat_a_nonzero_exit_as_an_error() {
        let mut runtime = runtime();
        let command = chars(&mut runtime, "printf out; exit 3");
        let out = call(&mut runtime, "os", "cmd", &[command])
            .expect("a non-zero exit status is not an error");
        assert_eq!(string_of(out), "out");
    }

    #[test]
    fn os_cmd_rejects_a_non_string_command() {
        let mut runtime = runtime();
        assert!(
            call(&mut runtime, "os", "cmd", &[]).is_err(),
            "arity 1 is required"
        );
        assert!(
            call(&mut runtime, "os", "cmd", &[make_small(7)]).is_err(),
            "an integer is not a command string"
        );
        assert!(
            call(&mut runtime, "os", "cmd", &[NIL]).is_ok(),
            "the empty string is a valid (if useless) command"
        );
    }

}