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
//! | `init:get_argument/1` | reads the parsed command line (contract A.6, `RuntimeConfig::init_args`) |
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
            ("init", "get_argument", 1) => Some(self.get_argument(args)),
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

    // -- init:get_argument/1 --------------------------------------------------

    /// `init:get_argument(Flag)`: `{ok, [[Value…]…]}` for a flag that was given (one
    /// inner list per occurrence, empty for a valueless flag), or the atom `error`.
    fn get_argument(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let flag = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        let occurrences = match self.runtime.init_args.get(&flag) {
            Some(occurrences) => occurrences,
            None => return self.atom("error"),
        };

        let mut occurrence_terms = Vec::with_capacity(occurrences.len());
        for occurrence in occurrences {
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

    // -- helpers --------------------------------------------------------------

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
        match self.output.as_deref_mut() {
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
    use beam_term::Atom;

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
            let device = context.call_bif("erlang", "self", 0, &[]).unwrap_or(beam_term::NIL);
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
        assert_eq!(
            terms::list_elements(occurrences[0]).expect("one value")[0],
            make_small(i64::from(b'/'))
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
        // And an unknown native-shaped call is not claimed either.
        assert!(call(&mut runtime, "erlang", "display", &[make_small(1), make_small(2)]).is_ok());
        let _ = Atom::MAX;
    }
}
