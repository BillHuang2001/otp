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
//! | `code:get_path/0`, `code:all_loaded/0`, `code:which/1`, `code:lib_dir/0,1` | read the runtime's own [`CodeServer`](crate::CodeServer) state (the code path, the loaded/preloaded module set, the installation) |
//!
//! The `init`/`code` calls are **preloaded-module surface** (contract A.7): the oracle
//! serves them from `init.beam`/`code.beam`, but those modules are written against the
//! process/message machinery Milestone-1 does not run, so this runtime answers them
//! directly from its own state instead of loading a module it cannot execute. `os:cmd/1`
//! is the same story for the spawn **driver** (contract A.4): the port machinery is a
//! later stage, but the host is perfectly able to spawn a shell.
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
            ("os", "cmd", 1) => Some(self.os_cmd(args)),
            ("code", "get_path", 0) => Some(self.code_get_path()),
            ("code", "all_loaded", 0) => Some(self.code_all_loaded()),
            ("code", "which", 1) => Some(self.code_which(args)),
            ("code", "lib_dir", 0) => Some(self.code_lib_dir_0()),
            ("code", "lib_dir", 1) => Some(self.code_lib_dir_1(args)),
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

    // -- init:get_argument/1 --------------------------------------------------

    /// `init:get_argument(Flag)`: `{ok, [[Value…]…]}` for a flag that was given (one
    /// inner list per occurrence, empty for a valueless flag), or the atom `error`.
    ///
    /// The occurrences are **snapshotted** out of the configuration before any term is
    /// built: every occurrence is owned data by then, so no borrow of the runtime is
    /// held across the mutable term construction below.
    fn get_argument(&mut self, args: &[Eterm]) -> Result<Eterm> {
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

    // -- init:get_plain_arguments/0 and init:get_arguments/0 ------------------

    /// `init:get_plain_arguments/0`: the arguments the emulator did not model, each as a
    /// string (contract A.6).
    ///
    /// This runtime has no `-extra` handling and the CLI rejects a stray positional
    /// argument outright (exit 2, see [`crate::cli::parse`]), so no plain argument can
    /// ever have been given: the result is always `[]` — which is exactly what the
    /// oracle reports for the corpus argv `["-noshell", …, "-eval", …]` (verified
    /// against OTP 29/erts-17.1).
    fn get_plain_arguments(&mut self) -> Result<Eterm> {
        Ok(NIL)
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
    fn get_arguments(&mut self) -> Result<Eterm> {
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

    // -- code:get_path/0, all_loaded/0, which/1, lib_dir/0,1 ------------------

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
    fn code_get_path(&mut self) -> Result<Eterm> {
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
    /// `Beam` is the atom `preloaded` for a preloaded module and its `.beam` file path
    /// (a string) for a loaded one. The preloaded names come first, so the list is never
    /// empty — as the oracle's always is. The oracle orders the list by load time; here
    /// the preloaded set is in boot order and the loaded modules are sorted by name
    /// (neither order is part of any contract).
    ///
    /// A module loaded **from memory** (no source file: [`CodeServer::load_bytes`]) is
    /// reported with the atom `preloaded` as its `Beam`: Milestone-1 records no load
    /// filename, and inventing one would be worse than the oracle's own
    /// `load_binary/3` filename hint, which this runtime does not take.
    ///
    /// [`CodeServer::load_bytes`]: crate::CodeServer::load_bytes
    fn code_all_loaded(&mut self) -> Result<Eterm> {
        // `None` marks a preloaded module (which has no `.beam` file of its own).
        let mut entries: Vec<(String, Option<String>)> = Vec::new();
        for name in self.runtime.preloaded().names() {
            entries.push((name.to_string(), None));
        }
        let loaded = self.runtime.code_server.loaded();
        for name in loaded {
            if self.runtime.preloaded().is_preloaded(&name) {
                continue;
            }
            let path = self.module_file(&name);
            entries.push((name, path));
        }

        let mut tuples = Vec::with_capacity(entries.len());
        for (name, path) in &entries {
            let name_atom = self.atom(name)?;
            let beam = match path {
                Some(path) => self.charlist(path)?,
                None => self.atom("preloaded")?,
            };
            tuples.push(terms::make_tuple(self, &[name_atom, beam])?);
        }
        terms::make_list(self, &tuples)
    }

    /// `code:which(Module)`: the atom `preloaded` for a preloaded module, the module's
    /// `.beam` path (a string) for one that is loaded or on the code path, and the atom
    /// `non_existing` otherwise.
    fn code_which(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let name = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        if self.runtime.preloaded().is_preloaded(&name) {
            return self.atom("preloaded");
        }
        match self.module_file(&name) {
            Some(path) => self.charlist(&path),
            None => self.atom("non_existing"),
        }
    }

    /// `code:lib_dir/0`: the system library root (`<lib>`) the code path's `ebin`
    /// directories live under, as a string.
    ///
    /// # Errors
    ///
    /// A diagnostic when the system code path cannot be resolved (a hard error: without
    /// it there is no `<lib>` to report) or holds no `<lib>/<app>-<vsn>/ebin` entry.
    fn code_lib_dir_0(&mut self) -> Result<Eterm> {
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
    fn code_lib_dir_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
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

    // -- helpers --------------------------------------------------------------

    /// `strings` as an Erlang list of strings (each a char list).
    fn string_list(&mut self, strings: &[String]) -> Result<Eterm> {
        let mut elems = Vec::with_capacity(strings.len());
        for text in strings {
            elems.push(self.charlist(text)?);
        }
        terms::make_list(self, &elems)
    }
    /// The name of the atom `term`, resolved through the runtime's shared atom table.
    fn atom_name(&self, term: Eterm) -> Option<String> {
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
    fn atom(&mut self, name: &str) -> Result<Eterm> {
        let atom = self.runtime.code_server.atoms_mut().intern(name);
        Ok(make_atom(atom))
    }

    /// `text` as an Erlang string (a char list), built on the engine's heap seam.
    fn charlist(&mut self, text: &str) -> Result<Eterm> {
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
    use crate::runtime::Runtime;

    /// A runtime over a code-path-less code server plus the core atom seed.
    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    /// Runs `module:function(args)` through the runtime's own dispatch (`call_bif`).
    fn call(runtime: &mut Runtime, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        context.call_bif(module, function, args.len() as u32, args)
    }

    fn atom(runtime: &mut Runtime, name: &str) -> Eterm {
        let atom = runtime.code_server_atoms_mut().intern(name);
        make_atom(atom)
    }

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
        assert_eq!(runtime.atom_name_of(terms::tuple_element(present, 0).unwrap()), Some("ok"));
        let values = terms::tuple_element(present, 1).unwrap();
        let occurrences = terms::list_elements(values).expect("the occurrence list");
        assert_eq!(occurrences.len(), 1);
        assert!(terms::list_elements(occurrences[0]).expect("no values").is_empty());

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
            vec![make_small(i64::from(b'/')), make_small(i64::from(b'a'))]
        );

        let absent = atom(&mut runtime, "absent");
        let missing = call(&mut runtime, "init", "get_argument", &[absent]).expect("absent");
        assert_eq!(runtime.atom_name_of(missing), Some("error"));
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

    // -- init:get_plain_arguments/0, init:get_arguments/0 ---------------------

    /// A runtime whose system code path resolved, or `None` (the caller then skips
    /// **loudly**) when no system Erlang is available.
    fn system_runtime() -> Option<Runtime> {
        let mut runtime = runtime();
        match runtime.code_server.add_system_paths() {
            Ok(()) => Some(runtime),
            Err(error) => {
                eprintln!("SKIP: no system code path ({error})");
                None
            }
        }
    }

    /// `text` as an Erlang string on the process heap (the same shape the floor returns).
    fn chars(runtime: &mut Runtime, text: &str) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        context.charlist(text).expect("a char list")
    }

    /// The Erlang string `term` as a Rust `String` (the inverse of [`chars`]).
    fn string_of(term: Eterm) -> String {
        terms::list_elements(term)
            .expect("a char list")
            .iter()
            .map(|character| {
                let code = u32::try_from(signed_value(*character)).expect("a code point");
                char::from_u32(code).expect("a char")
            })
            .collect()
    }

    #[test]
    fn get_plain_arguments_is_always_empty() {
        // The CLI models no plain arguments (a stray positional argument is a usage
        // error), so `init:get_plain_arguments/0` is the empty list, as the oracle's is.
        let mut runtime = runtime();
        let plain = call(&mut runtime, "init", "get_plain_arguments", &[]).expect("plain args");
        assert_eq!(plain, NIL);
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
                    runtime.atom_name_of(flag).expect("the flag is an atom").to_string(),
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

    // -- code:get_path/0, all_loaded/0, which/1, lib_dir/0,1 ------------------

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
        // The preloaded set comes first, starting with `erlang`.
        let first = entries[0];
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(first, 0).unwrap()),
            Some("erlang")
        );
        assert_eq!(
            runtime.atom_name_of(terms::tuple_element(first, 1).unwrap()),
            Some("preloaded")
        );
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
        assert_eq!(string_of(terms::tuple_element(slice_entry, 1).unwrap()), slice);
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
        let answer = call(&mut runtime, "code", "lib_dir", &[make_small(42)])
            .expect("code:lib_dir/1");
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
        assert!(app_dir.starts_with(&root), "{root} is not a prefix of {app_dir}");

        // An application that is not installed is `{error, bad_name}`.
        let missing = atom(&mut runtime, "definitely_not_an_otp_app");
        let answer =
            call(&mut runtime, "code", "lib_dir", &[missing]).expect("code:lib_dir/1");
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
}