//! The port half of the runtime-native BIF floor: `erlang:port_command/2,3` and the
//! closed-port half of `erlang:port_info/1,2`.
//!
//! `beam_bifs` owns the *roster* of port BIFs (`open_port/2`, `port_close/1`,
//! `port_info/1,2`, `list_to_port/1`, `port_to_list/1` — see
//! `../../beam_bifs/src/port.rs`) and reaches the port *machinery* through the
//! [`PortHook`](beam_bifs::PortHook) seam, which [`crate::port::RuntimePorts`] implements.
//! Two things do **not** fit that seam and are served here instead, ahead of the registry:
//!
//! | Call | Why it is runtime-native |
//! |---|---|
//! | `erlang:port_command/2,3` | writing to the child is the port's *input* direction, which the `PortHook` seam has no method for — the runtime owns the child's stdin handle |
//! | `erlang:port_info/1,2` on a port the runtime does not hold | the answer is the atom `undefined` (see below), which a `PortHook` returning `{Item, Value}` pairs cannot express |
//!
//! # `port_command/2,3`
//!
//! `Data` is Erlang **iodata**: a binary, a char list, or a nested iolist of
//! integers/binaries (`io_lib:format/2` output is the common case). It is written to the
//! child's stdin, which is what makes `open_port({spawn,"cat"},[])` echo its input back —
//! the read side is [`RuntimeContext::poll_port_messages`](crate::context::RuntimeContext).
//! The result is the atom `true` (the oracle's own value). A non-port argument, a port the
//! runtime does not hold (closed, or never opened), and non-iodata are
//! [`BeamError::BadArg`] — the oracle's reason; a port with no child stdin of ours (a
//! `{fd, N}` port) is a typed *capability gap*, never a silent write elsewhere.
//!
//! # `port_info/1,2` on a closed port
//!
//! The oracle answers `undefined` for a port that is no longer live — `erlang.erl`'s own
//! wrapper maps erts's empty item list to `undefined` for both arities. `beam_bifs` builds
//! `port_info/1` from the hook's pairs as a plain list (an empty pair list is `[]`, not
//! `undefined`), so the native floor **claims only that case**: a live port returns `None`
//! here and falls through to the registry unchanged, where `beam_bifs`'s own `port_info/1`
//! and `port_info/2` (including its `undefined`-for-an-unknown-*item* rule) serve it. No
//! item vocabulary or `=:=` lookup is duplicated.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_process::PortId;
use beam_term::{is_small, signed_value, Eterm};

use crate::context::RuntimeContext;
use crate::error;

impl RuntimeContext<'_> {
    /// The single entry the [`try_native`](RuntimeContext::try_native) dispatch arm added
    /// for the port surface calls: `erlang:port_command/2,3` and the closed-port half of
    /// `erlang:port_info/1,2`.
    ///
    /// Anything this floor does not claim — every other `(function, arity)` of the two
    /// names — is `None`, i.e. "not a native BIF, fall through to the `beam_bifs`
    /// registry", exactly as if the arm did not exist.
    pub(super) fn port_native(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (function, arity) {
            ("port_command", 2) => Some(self.port_command(args, false)),
            ("port_command", 3) => Some(self.port_command(args, true)),
            ("port_info", 1) | ("port_info", 2) => self.port_info_closed_port(args),
            _ => None,
        }
    }

    /// `erlang:port_command(Port, Data[, Options])`: write `Data`'s bytes to the port's
    /// child and answer the atom `true`.
    ///
    /// The `/3` `Options` are validated as a list (`[force, nosuspend]` and friends) and
    /// otherwise **ignored**: none of them changes what an asynchronous, always-buffered
    /// write here does, and rejecting one the real driver accepts would be worse than
    /// accepting it as a no-op (the same rule `open_port/2`'s options follow — see
    /// [`crate::port`]).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-port, a port the runtime does not hold, a
    /// non-list `Options` or non-iodata `Data`; a capability gap for a port that owns no
    /// child stdin; [`BeamError::Io`] when the write fails.
    fn port_command(&mut self, args: &[Eterm], with_options: bool) -> Result<Eterm> {
        let port = PortId::from_eterm(*args.first().ok_or(BeamError::BadArg)?)?;
        let data = *args.get(1).ok_or(BeamError::BadArg)?;
        if with_options {
            // A malformed option container is badarg; the options themselves are a no-op.
            terms::list_elements(*args.get(2).ok_or(BeamError::BadArg)?)?;
        }
        let mut bytes = Vec::new();
        self.iodata_bytes(data, &mut bytes)?;
        // The registry is reached through a cheap clone of its `Rc` handle so writing to the
        // child never holds a borrow of the runtime while a term (the atom `true`) is built —
        // the same snapshot rule the port poller follows (`crate::port`).
        let ports = std::rc::Rc::clone(&self.runtime.ports);
        {
            let mut registry = ports
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.write_command(port, &bytes)?;
        }
        self.atom("true")
    }

    /// `erlang:port_info/1,2` for a port the runtime does **not** hold: `Some(Ok(undefined))`
    /// — the oracle's answer for a closed port.
    ///
    /// `None` (fall through to `beam_bifs`) when the argument is not a port term at all (the
    /// registry's own `badarg` is the oracle's answer there) and when the port **is** live
    /// (its item list is the registry's).
    pub(super) fn port_info_closed_port(&mut self, args: &[Eterm]) -> Option<Result<Eterm>> {
        let port = args.first().copied().and_then(|term| PortId::from_eterm(term).ok())?;
        let live = {
            let registry = self.runtime.ports.try_borrow().ok()?;
            registry.is_live(port)
        };
        if live {
            return None;
        }
        Some(self.atom("undefined"))
    }

    /// Collects `term` as Erlang **iodata** into `out`: a byte-sized small integer, a binary,
    /// or a (possibly nested) list of those.
    ///
    /// This is `iolist_to_binary/1`'s acceptance rule, restricted to what the port stream
    /// can carry: a non-integer, an out-of-range integer, an improper tail and a non-iodata
    /// term are all [`BeamError::BadArg`]. Unlike the printer helper `chars_to_string` (a
    /// `String`, so bytes must be UTF-8), the port stream is a **byte** stream: a binary of
    /// arbitrary bytes goes through untouched, so a `port_command` of `<<255,0,1>>` writes
    /// exactly those three bytes.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a term that is not iodata.
    fn iodata_bytes(&self, term: Eterm, out: &mut Vec<u8>) -> Result<()> {
        if is_small(term) {
            out.push(u8::try_from(signed_value(term)).map_err(|_| BeamError::BadArg)?);
            return Ok(());
        }
        if let Ok(bytes) = terms::binary_bytes(term) {
            out.extend_from_slice(bytes);
            return Ok(());
        }
        if let Ok(elements) = terms::list_elements(term) {
            for element in elements {
                self.iodata_bytes(element, out)?;
            }
            return Ok(());
        }
        Err(BeamError::BadArg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::test_support::{atom, call, chars, runtime};
    use beam_bifs::NullSink;
    use beam_term::{make_small, NIL};

    /// An iolist `[72, <<"i">>, [33]]` (⇒ `"Hi!"`) built on the context's heap.
    fn iodata(context: &mut RuntimeContext<'_>) -> Eterm {
        let inner = terms::make_list(&mut *context, &[make_small(33)]).expect("a nested list");
        let binary = terms::make_heap_binary(&mut *context, b"i").expect("a binary");
        terms::make_list(&mut *context, &[make_small(72), binary, inner]).expect("an iolist")
    }

    #[test]
    fn port_command_writes_the_char_list_bytes_and_answers_true() {
        let mut runtime = runtime();
        // Open a real `cat` through the runtime's own port table, then write to it. (What
        // the child does with the bytes — `cat` echoes them — is asserted through the
        // port's own stream handle in `crate::port`'s tests, and through the poller at the
        // CLI boundary in `tests/eval.rs`.)
        let name = {
            let mut sink = NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            let command = context.charlist("cat").expect("a command");
            let spawn = context.atom("spawn").expect("the spawn atom");
            terms::make_tuple(&mut context, &[spawn, command]).expect("a name tuple")
        };
        let port = call(&mut runtime, "erlang", "open_port", &[name, NIL]).expect("open_port");
        let port = PortId::from_eterm(port).expect("a port term");
        let data = chars(&mut runtime, "hello");

        let answer = call(&mut runtime, "erlang", "port_command", &[port.to_eterm(), data])
            .expect("port_command/2");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));

        // The port is still live (a write never closes it) and its child stdin was consumed
        // by the write rather than by `open_port`/`port_close`.
        let ports = std::rc::Rc::clone(&runtime.ports);
        let registry = ports.try_borrow().expect("the port table");
        assert!(registry.is_live(port), "port_command must not close the port");
    }

    #[test]
    fn port_command_rejects_a_non_port_a_dead_port_and_non_iodata() {
        let mut runtime = runtime();
        let data = chars(&mut runtime, "x");

        // A non-port argument.
        assert!(matches!(
            call(&mut runtime, "erlang", "port_command", &[make_small(1), data]),
            Err(BeamError::BadArg)
        ));
        // A port the runtime does not hold (never opened, i.e. closed from here).
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data]
            ),
            Err(BeamError::BadArg)
        ));
        // Non-iodata: an atom is neither an integer, a binary nor a list.
        let bad = atom(&mut runtime, "not_iodata");
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), bad]
            ),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn port_command_3_tolerates_a_list_of_options() {
        let mut runtime = runtime();
        let data = chars(&mut runtime, "x");
        // `nosuspend` is a real `port_command/3` option; the write itself still happens
        // (here: it fails on a dead port, so the option list was clearly *not* the reason).
        let option = atom(&mut runtime, "nosuspend");
        let options = terms::make_list(
            &mut RuntimeContext::new(&mut runtime),
            &[option],
        )
        .expect("an option list");
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data, options]
            ),
            Err(BeamError::BadArg)
        ));
        // A non-list option container is a different badarg: the `/3` arity validates it.
        let bad = atom(&mut runtime, "not_options");
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data, bad]
            ),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn port_info_on_an_unknown_port_is_undefined() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        for arity in [1usize, 2] {
            let mut args = vec![PortId::new(42).to_eterm()];
            if arity == 2 {
                args.push(make_small(1));
            }
            let answer = context
                .port_info_closed_port(&args)
                .unwrap_or_else(|| panic!("port_info/{arity} must claim an unknown port"))
                .expect("the atom undefined");
            assert_eq!(context.atom_name(answer).as_deref(), Some("undefined"));
        }
        // A non-port argument falls through (the registry's `badarg` is the oracle's answer).
        assert!(context.port_info_closed_port(&[make_small(1)]).is_none());
    }

    #[test]
    fn port_native_claims_only_the_command_and_closed_info_pairs() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        // The arities this floor does *not* claim fall through untouched.
        for (function, arity) in [("port_command", 1), ("port_info", 0), ("port_info", 3)] {
            assert!(
                context.port_native(function, arity, &[]).is_none(),
                "{function}/{arity} must not be claimed"
            );
        }
    }

    #[test]
    fn iodata_bytes_flattens_integers_binaries_and_nested_lists() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        let term = iodata(&mut context);
        let mut out = Vec::new();
        context.iodata_bytes(term, &mut out).expect("an iolist");
        assert_eq!(out, b"Hi!");

        // The empty list is iodata and contributes nothing.
        let mut out = Vec::new();
        context.iodata_bytes(NIL, &mut out).expect("[]");
        assert!(out.is_empty());
    }

    #[test]
    fn iodata_rejects_non_iodata_and_out_of_range_integers() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        let mut out = Vec::new();
        // An atom, and a small integer that is not a byte.
        let bad = context.atom("not_iodata").expect("an atom");
        assert!(matches!(
            context.iodata_bytes(bad, &mut out),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            context.iodata_bytes(make_small(256), &mut out),
            Err(BeamError::BadArg)
        ));
    }
}