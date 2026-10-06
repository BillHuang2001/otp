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
//!
//! `/3` `Options` are validated **and honored**: `nosuspend` changes nothing here (the write
//! is always asynchronous and buffered), while `force` is rejected with the oracle's
//! `error:notsup` because the spawn driver does not carry `ERL_DRV_FLAG_SOFT_BUSY`. Every
//! other option — an unknown atom, a non-atom element, an improper tail or a non-list
//! container — is `error:badarg`, and the port liveness check precedes the `force` check, so
//! a dead port with `[force]` is `badarg` rather than `notsup`. The result is the atom
//! `true` (the oracle's own value). A non-port argument, a port the runtime does not hold
//! (closed, or never opened), and non-iodata are [`BeamError::BadArg`] — the oracle's
//! reason; a port with no child stdin of ours (a `{fd, N}` port) is a typed *capability
//! gap*, never a silent write elsewhere.
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
    /// The steps follow the oracle's own order (`erts_internal_port_command_3`, with the
    /// `badopt` → `badarg` mapping `erlang.erl`'s `port_command/3` wrapper applies):
    ///
    /// 1. decode the port (a non-port is [`BeamError::BadArg`]);
    /// 2. validate **every** `/3` option ([`RuntimeContext::port_command_options`]);
    /// 3. the port must still be live — the same check the `/2` path makes, and it
    ///    deliberately precedes the `force` check, so a dead port with `[force]` is `badarg`
    ///    rather than `notsup`;
    /// 4. `force` — a request to bypass the driver's busy state — is `error:notsup`, because
    ///    the spawn driver does not carry `ERL_DRV_FLAG_SOFT_BUSY`;
    /// 5. otherwise collect `Data` as iodata and write it.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-port, a port the runtime does not hold, a malformed
    /// or unrecognized option list, or non-iodata `Data`; the `notsup` raise for `force`; a
    /// capability gap for a port that owns no child stdin; [`BeamError::Io`] when the write
    /// fails.
    fn port_command(&mut self, args: &[Eterm], with_options: bool) -> Result<Eterm> {
        let port = PortId::from_eterm(*args.first().ok_or(BeamError::BadArg)?)?;
        let data = *args.get(1).ok_or(BeamError::BadArg)?;
        let force = if with_options {
            self.port_command_options(*args.get(2).ok_or(BeamError::BadArg)?)?
        } else {
            false
        };
        // The registry is reached through a cheap clone of its `Rc` handle so writing to the
        // child never holds a borrow of the runtime while a term (the atom `true`) is built —
        // the same snapshot rule the port poller follows (`crate::port`).
        let ports = std::rc::Rc::clone(&self.runtime.ports);
        {
            let registry = ports
                .try_borrow()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            if !registry.is_live(port) {
                return Err(BeamError::BadArg);
            }
        }
        if force {
            // `force` asks the driver to push the command through a busy port regardless.
            // The spawn driver does not advertise `ERL_DRV_FLAG_SOFT_BUSY`, so the oracle
            // answers the atom `notsup` — an **error**-class raise, deliberately not a
            // capability gap (see `crate::error`'s `unsupported` convention).
            return Err(error::message("notsup"));
        }
        let mut bytes = Vec::new();
        self.iodata_bytes(data, &mut bytes)?;
        {
            let mut registry = ports
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.write_command(port, &bytes)?;
        }
        self.atom("true")
    }

    /// Validates `erlang:port_command/3`'s `Options` and reports whether `force` was
    /// requested.
    ///
    /// The oracle accepts exactly the atoms `force` and `nosuspend` as elements
    /// (`erts_internal_port_command_3`'s `is_not_nil`/`is_list` loop) and maps everything
    /// else to `badarg`: an unknown atom, a non-atom element, an **improper** tail
    /// (`[force|bogus]`) and a non-list container all fail. `[]` and `nosuspend` change
    /// nothing about the write here (it is always asynchronous and buffered), so only
    /// `force` is reported back — the caller rejects it as `notsup`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a malformed option container/tail or an unrecognized
    /// option element.
    fn port_command_options(&self, options: Eterm) -> Result<bool> {
        let mut force = false;
        for element in terms::list_elements(options).map_err(|_| BeamError::BadArg)? {
            match self.atom_name(element).as_deref() {
                Some("force") => force = true,
                Some("nosuspend") => {}
                _ => return Err(BeamError::BadArg),
            }
        }
        Ok(force)
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
    use crate::runtime::Runtime;
    use beam_bifs::NullSink;
    use beam_term::{make_small, NIL};

    /// An iolist `[72, <<"i">>, [33]]` (⇒ `"Hi!"`) built on the context's heap.
    fn iodata(context: &mut RuntimeContext<'_>) -> Eterm {
        let inner = terms::make_list(&mut *context, &[make_small(33)]).expect("a nested list");
        let binary = terms::make_heap_binary(&mut *context, b"i").expect("a binary");
        terms::make_list(&mut *context, &[make_small(72), binary, inner]).expect("an iolist")
    }

    /// A real, **live** `cat` port opened through the runtime's own `open_port/2` — the
    /// same child the write test below drives (and the one `cat` echoes back).
    fn live_port(runtime: &mut Runtime) -> PortId {
        let name = {
            let mut sink = NullSink;
            let mut context = RuntimeContext::with_output(runtime, &mut sink);
            let command = context.charlist("cat").expect("a command");
            let spawn = context.atom("spawn").expect("the spawn atom");
            terms::make_tuple(&mut context, &[spawn, command]).expect("a name tuple")
        };
        let port = call(runtime, "erlang", "open_port", &[name, NIL]).expect("open_port");
        PortId::from_eterm(port).expect("a port term")
    }

    /// `elements` as a list on the runtime's heap (the `/3` option-container shape).
    fn list_of(runtime: &mut Runtime, elements: &[Eterm]) -> Eterm {
        terms::make_list(&mut RuntimeContext::new(runtime), elements).expect("a list")
    }

    /// The **error**-class raise reason atom of `error`, panicking when it is not one.
    ///
    /// The floor builds its message raises with [`crate::error::message`], whose reason is an
    /// atom backed by an owned `String`; [`BeamError::as_atom`] must return a `&'static str`
    /// and so reports `None` for it — the name has to be read through
    /// [`ExitReason::as_atom`](beam_common::ExitReason::as_atom).
    fn raise_atom(error: &BeamError) -> String {
        match error {
            BeamError::Raise(raise) => {
                assert_eq!(
                    raise.class,
                    beam_common::ExceptionClass::Error,
                    "the oracle raises `error`: {raise}"
                );
                raise
                    .reason
                    .as_atom()
                    .unwrap_or_else(|| panic!("a non-atom reason: {raise}"))
                    .to_string()
            }
            other => panic!("not a raise: {other}"),
        }
    }

    #[test]
    fn port_command_writes_the_char_list_bytes_and_answers_true() {
        let mut runtime = runtime();
        // Open a real `cat` through the runtime's own port table, then write to it. (What
        // the child does with the bytes — `cat` echoes them — is asserted through the
        // port's own stream handle in `crate::port`'s tests, and through the poller at the
        // CLI boundary in `tests/eval.rs`.)
        let port = live_port(&mut runtime);
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
        // `nosuspend` is a real `port_command/3` option and is accepted; the write itself
        // still happens (here: it fails the port-liveness check on a dead port, so the
        // option list was clearly *not* the reason) — the liveness check precedes both the
        // `force` check and the write, exactly as the oracle looks the port up first.
        let option = atom(&mut runtime, "nosuspend");
        let options = list_of(&mut runtime, &[option]);
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data, options]
            ),
            Err(BeamError::BadArg)
        ));
        // An empty option list is fine too (the `/2` arity is `port_command(P, D, [])`).
        let empty = list_of(&mut runtime, &[]);
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data, empty]
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

    /// `[force]` on a **live** port: the spawn driver does not carry
    /// `ERL_DRV_FLAG_SOFT_BUSY`, so the oracle raises `error:notsup` (never `badarg`, and
    /// never a capability gap — `notsup` does not start with `unsupported`).
    #[test]
    fn port_command_3_force_on_a_live_port_is_notsup() {
        let mut runtime = runtime();
        let port = live_port(&mut runtime);
        let data = chars(&mut runtime, "x");
        let force = atom(&mut runtime, "force");
        let nosuspend = atom(&mut runtime, "nosuspend");
        for options in [
            list_of(&mut runtime, &[force]),
            list_of(&mut runtime, &[force, nosuspend]),
            list_of(&mut runtime, &[nosuspend, force]),
        ] {
            let error = call(
                &mut runtime,
                "erlang",
                "port_command",
                &[port.to_eterm(), data, options],
            )
            .expect_err("`force` is not supported by the spawn driver");
            assert_eq!(raise_atom(&error), "notsup", "{error}");
            // `notsup` is a genuine error, **not** a capability gap: the differential
            // harness turns any `unsupported`-prefixed reason into the exit-3 signal, and
            // the oracle exits non-zero with `error:notsup`.
            assert_eq!(error::unsupported_reason(&error), None, "{error}");
        }
        // Rejecting `force` neither closes the port nor consumes it: a plain `/2` write
        // still succeeds afterwards.
        let answer = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), data],
        )
        .expect("port_command/2 after the notsup");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
    }

    /// A live port with a malformed/unrecognized option list is `badarg` — the oracle's
    /// `badopt`, which `erlang.erl`'s `port_command/3` wrapper maps onto `badarg`. This is
    /// checked **before** `force` would be honored, so `[force|bogus]` is `badarg`, not
    /// `notsup`.
    #[test]
    fn port_command_3_rejects_unknown_and_malformed_options_on_a_live_port() {
        let mut runtime = runtime();
        let port = live_port(&mut runtime);
        let data = chars(&mut runtime, "x");
        let force = atom(&mut runtime, "force");
        let bogus = atom(&mut runtime, "bogus");
        let one = make_small(1);
        let not_a_list = atom(&mut runtime, "not_options");
        // `[force|bogus]`: a cons whose tail is an atom, i.e. an **improper** list.
        let improper = terms::make_cons(&mut RuntimeContext::new(&mut runtime), force, bogus)
            .expect("an improper list");
        for (label, options) in [
            ("an unknown atom", list_of(&mut runtime, &[bogus])),
            (
                "`force` mixed with an unknown atom",
                list_of(&mut runtime, &[force, bogus]),
            ),
            ("a non-atom element", list_of(&mut runtime, &[one])),
            ("an improper tail", improper),
            ("a non-list container", not_a_list),
        ] {
            match call(
                &mut runtime,
                "erlang",
                "port_command",
                &[port.to_eterm(), data, options],
            ) {
                Err(BeamError::BadArg) => {}
                Err(other) => panic!("{label} must be badarg, got {other}"),
                Ok(_) => panic!("{label} must not be accepted"),
            }
        }
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