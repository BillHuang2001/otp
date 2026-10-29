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

//! The walker's **raise-trace model**: the one item it can derive, and what it
//! deliberately does not render.
//!
//! # What a raise carries
//!
//! [`Flow::Raise`](super::Flow) transports a raise's class, its live reason term,
//! the original [`BeamError`] (`origin`, set for anything that came back from a
//! BIF) and the walker's single derived **raise-site item** (`head`, see
//! [`call_site_item`]).
//!
//! # The raise-site item
//!
//! ERTS renders a raise's innermost stacktrace item from state the *raising call*
//! owns: the BIF puts its own real argument list in the item's `A` slot
//! (`../../erts/emulator/beam/beam_common.c`'s `build_stacktrace`), and
//! `erlang:error/2,3` instead substitute the argument list they were **given**
//! (C's `non_bif_stacktrace`, `bif.c:1448-1462`). The tree-walker is the failing
//! call site, so it is the one place that knows both, and it renders exactly that
//! item:
//!
//! ```text
//! {CalleeModule, CalleeFunction, A, Location}
//! ```
//!
//! * `CalleeModule`/`CalleeFunction` — the atoms the walker actually dispatched
//!   (`hd(1)` ⇒ `erlang`/`hd`; `lists:max([])` ⇒ `lists`/`max`).
//! * `A` — the raise's **`saved_args`** term when the raise carries one (the
//!   `erlang:error/2,3` substitution, read verbatim and never validated: a
//!   non-list argument is that term), otherwise the walker's **real evaluated
//!   argument list**.
//! * `Location` — `[]`, or the one-element list `[{error_info, Map}]` when the
//!   raise carries an `error_info` map (`erlang:error/3`'s options, scanned by
//!   `beam_bifs` — the C appends it to the frame's location list).
//!
//! [`attach_call_site`] therefore hands back the raise with `head` filled in, and
//! it does so for **every** raise a call returns — not only the bare `undef` it
//! used to be limited to — as long as the raise has no item yet (an inner call's
//! item wins, and a re-raise keeps the item it carries) and no caller-supplied
//! trace ([`cook_trace`]'s echo rule).
//!
//! # What is deliberately *not* rendered
//!
//! The oracle's `-eval` route renders its own frames as well — `erl_eval:do_apply/7`,
//! `erl_eval:expr/6`, `init:start_it/1`, …, each with a `{file, _}`/`{line, _}`
//! location. Those are **OTP source facts**: they name functions in `erl_eval.erl`
//! and `init.erl` at line numbers that move between OTP releases (the tree's pinned
//! release is OTP 30, the host oracle is 29.x), and this route runs no `erl_eval`
//! port at all. Fabricating them would be a wrong term, so the derived item is the
//! whole trace and the chain stays a documented residual (`../CONTEXT.md`).
//!
//! Three further consequences of the same rule:
//!
//! * a `{file, _}`/`{line, _}` source position is likewise unavailable — the
//!   walker has no Erlang source position, so `Location` is only ever `[]` or the
//!   `error_info` extension;
//! * the item's identity is the **raise site** (`{erlang, error, …}` for
//!   `erlang:error/1,2,3`), where OTP names the *caller* (`erl_eval:do_apply/7`);
//! * a BIF that raises through `error_with_info/2` gains an
//!   `{error_info, #{module => FormatModule}}` extension from a closed table that
//!   lives `pub(crate)` in `beam_engine`'s `families::exceptions::bif_frame`, so a
//!   stdlib/BIF raise here keeps an empty `Location`.
//!
//! `exit/1`, `throw/1`, `nif_error/1,2` and a fun application (`apply/2,3`, a
//! closure, `fun m:f/A`) contribute no item of their own: `exit`/`throw` render no
//! trace by class rule, `nif_error`'s second argument is crash-report metadata (it
//! must never appear in an `A` slot), and `apply` dispatches a target that the
//! callee's own [`attach_call_site`] already names.
//!
//! # Carrying the item across the evaluator's escape boundary
//!
//! A raise that nothing in the evaluated expression catches leaves the walker as a
//! [`BeamError`] (`mod.rs`'s [`finish`](super::finish)), and that is precisely the
//! case where a caller *outside* this expression may still render a trace — the
//! `-eval` driver's CLI report, or a loaded module that applied one of this
//! evaluator's closures and caught the raise itself. The `Flow` is gone by then, so
//! [`carry_derived_item`] moves the item onto the raise's supplied-trace slot as
//! the one-element list `[item]`; [`cook_trace`] then echoes it like any supplied
//! trace and no outer call site pre-pends an item of its own. A raise with no item,
//! one that already carries a supplied trace, and a term-building failure all pass
//! through untouched.
//!
//! # The supplied-trace echo
//!
//! `erlang:raise(Class, Reason, Stacktrace)` supplies the whole trace, and ERTS
//! echoes it **verbatim** (C's `build_stacktrace` `is_raised_exc` arm,
//! `beam_common.c:1219-1223`). [`cook_trace`] therefore returns the supplied term
//! unchanged and ignores any derived item — which is also what makes the echo exact
//! for the shapes a re-encoding would lose (`[]` as an explicitly empty trace, a
//! frame whose `A` slot is not an argument list, an improper list).

use beam_common::{BeamError, LiveReason, Raise, Result};
use beam_engine::{terms, ExecutionContext};
use beam_term::Eterm;

use super::{EResult, Evaluator, Flow};

/// The cooked trace of a raise: its derived raise-site item, or the frames a
/// caller supplied to `erlang:raise/3` **verbatim**.
///
/// * A supplied trace ([`Raise::supplied_trace`]) wins outright and no derived item
///   is consed in front of it — the echo is the C's, unchanged.
/// * Otherwise the raise's own `stacktrace` is rendered by the engine's public
///   [`encode_supplied_stacktrace`](beam_engine::encode_supplied_stacktrace) (the
///   frames a nested compiled call contributed, e.g. an applied external fun or an
///   engine-side raise) and the derived item is consed **in front of** it, so the
///   innermost frame comes first exactly as the oracle renders it.
///
/// A raise with neither renders `[]` — never a fabricated `erl_eval`/`init` frame
/// (the module docs say why).
///
/// The `module` handed to the encoder is an **atom source only** — resolved by
/// lookup, nothing is interned — so it carries a **fork** of the runtime's shared
/// atom table (built after the raise's frame names were already interned by the
/// `-eval` expression, so it names exactly what the shared table names at every
/// index); a name the table cannot resolve surfaces as the typed
/// `error::unsupported(...)`, never a substituted atom.
pub(super) fn cook_trace(
    evaluator: &mut Evaluator<'_, '_>,
    origin: &Option<BeamError>,
    head: Option<Eterm>,
) -> EResult<Eterm> {
    let raise = match origin {
        Some(BeamError::Raise(raise)) => Some(raise),
        _ => None,
    };
    if let Some(raise) = raise {
        if raise.supplied_trace().is_some() {
            // The raise's own frames were decoded from the supplied trace at the raise
            // (`beam_bifs::convert::raise_3`), so a **valid** frame list is re-rendered
            // through the engine's encoder — the oracle's normalisation, which gives a
            // 3-tuple frame `{M,F,A}` the location slot the oracle gives it
            // (`{M,F,A,[]}`) while a 4-tuple frame round-trips unchanged. An **empty**
            // frame list (`[]`, the raise's "no trace" case) echoes the term itself.
            if raise.stacktrace.is_empty() {
                if let Some(trace) = supplied_trace_term(raise) {
                    return Ok(trace);
                }
            } else {
                let module = beam_loader::Module {
                    atom_table: evaluator.ctx.runtime.code_server.atoms().fork(),
                    ..beam_loader::Module::default()
                };
                let ctx: &mut dyn ExecutionContext = &mut *evaluator.ctx;
                return term(beam_engine::encode_supplied_stacktrace(
                    &module,
                    ctx,
                    &raise.stacktrace,
                ));
            }
        }
    }
    let mut trace = match raise {
        Some(raise) => {
            let module = beam_loader::Module {
                atom_table: evaluator.ctx.runtime.code_server.atoms().fork(),
                ..beam_loader::Module::default()
            };
            let ctx: &mut dyn ExecutionContext = &mut *evaluator.ctx;
            term(beam_engine::encode_supplied_stacktrace(
                &module,
                ctx,
                &raise.stacktrace,
            ))?
        }
        None => beam_term::NIL,
    };
    if let Some(item) = head {
        trace = term(terms::make_cons(evaluator.ctx, item, trace))?;
    }
    Ok(trace)
}

/// Attaches the walker's derived raise-site item to `flow` when it is a raise that
/// has none of its own, returning the (possibly updated) flow.
///
/// The gates are the whole contract (see the module docs):
///
/// * the flow must be a [`Flow::Raise`] whose `head` is still `None` — an inner
///   call's item wins and a re-raise keeps the item it carries;
/// * the raise must carry **no** supplied `erlang:raise/3` trace — that trace is
///   echoed verbatim and must never gain a frame;
/// * building the item must succeed: a term-building failure leaves the raise's
///   class/reason untouched (today's trace) rather than masking the real exception
///   with a `system_limit`.
///
/// `module`/`function` are atom **immediates** and `args` the evaluated argument
/// list of the call site being reported.
pub(super) fn attach_call_site(
    evaluator: &mut Evaluator<'_, '_>,
    flow: Flow,
    module: Eterm,
    function: Eterm,
    args: &[Eterm],
) -> Flow {
    let Flow::Raise {
        class,
        reason,
        origin,
        head: None,
    } = flow
    else {
        return flow;
    };
    if carries_supplied_trace(&origin) {
        return Flow::Raise {
            class,
            reason,
            origin,
            head: None,
        };
    }
    match call_site_item(evaluator, module, function, args, &origin) {
        Ok(item) => Flow::Raise {
            class,
            reason,
            origin,
            head: Some(item),
        },
        Err(_) => Flow::Raise {
            class,
            reason,
            origin,
            head: None,
        },
    }
}

/// [`attach_call_site`] for a call named by its `module`/`function` **strings**.
///
/// The atoms are interned here, and only once the raise is known to be attachable,
/// so a failing call that already carries an item (the common case inside a nested
/// expression) interns nothing. An `apply/3` whose module/function came from a term
/// reaches [`attach_call_site`] directly, where interning is a no-op lookup.
pub(super) fn attach_call_site_str(
    evaluator: &mut Evaluator<'_, '_>,
    flow: Flow,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Flow {
    if !attachable(&flow) {
        return flow;
    }
    let module = evaluator.atom(module);
    let function = evaluator.atom(function);
    attach_call_site(evaluator, flow, module, function, args)
}

/// Whether [`attach_call_site`] would change `flow`: a raise with no item and no
/// caller-supplied trace.
fn attachable(flow: &Flow) -> bool {
    match flow {
        Flow::Raise { origin, head: None, .. } => !carries_supplied_trace(origin),
        _ => false,
    }
}

/// Whether the raise behind `origin` carries the trace an `erlang:raise/3` caller
/// supplied — the discriminator the echo rule turns on (never
/// `stacktrace.is_empty()`: an explicitly empty supplied trace is still a supplied
/// trace).
fn carries_supplied_trace(origin: &Option<BeamError>) -> bool {
    match origin {
        Some(BeamError::Raise(raise)) => raise.supplied_trace().is_some(),
        _ => false,
    }
}

/// The item `{Module, Function, A, Location}` for the callee applied to `args`.
///
/// `A` is the raise's `saved_args` term when it has one and `args` (one cons cell
/// per argument) otherwise; `Location` is `[]` or `[{error_info, Map}]`. One
/// 4-element tuple plus the list cells.
fn call_site_item(
    evaluator: &mut Evaluator<'_, '_>,
    module: Eterm,
    function: Eterm,
    args: &[Eterm],
    origin: &Option<BeamError>,
) -> Result<Eterm> {
    let args_slot = match live_slot(origin, Raise::saved_args) {
        Some(saved) => saved,
        None => terms::make_list(evaluator.ctx, args)?,
    };
    let location = match live_slot(origin, Raise::error_info) {
        Some(map) => {
            let error_info = evaluator.atom("error_info");
            let pair = terms::make_tuple(evaluator.ctx, &[error_info, map])?;
            terms::make_list(evaluator.ctx, &[pair])?
        }
        None => beam_term::NIL,
    };
    terms::make_tuple(evaluator.ctx, &[module, function, args_slot, location])
}

/// Carries the walker's derived raise-site item across the evaluator's **escape
/// boundary** — `mod.rs`'s `finish`, the one place a `Flow::Raise` becomes a
/// `BeamError`.
///
/// Without this the item would die with the `Flow`: an escape happens exactly when
/// nobody in this expression caught the raise (the `-eval` driver hands it to the
/// CLI) or when a loaded module applied one of this evaluator's closures
/// ([`apply_eval_closure`](super::apply_eval_closure) returns the error to its
/// caller, which may still be inside a statement that catches it). Both are cases
/// where the raise travels **on** the `BeamError`, so the item rides the raise's
/// supplied-trace slot as the one-element trace list `[item]`: [`cook_trace`] then
/// echoes it verbatim (as it does any supplied trace) and no outer call site
/// pre-pends an item of its own.
///
/// The gates mirror [`attach_call_site`]'s: the item must exist, the raise must not
/// already carry a supplied trace, and a term-building failure leaves the raise
/// exactly as it was. A non-`Raise` origin carries no slot to put an item in, so it
/// is returned untouched.
pub(super) fn carry_derived_item(
    ctx: &mut crate::context::RuntimeContext<'_>,
    origin: Option<BeamError>,
    head: Option<Eterm>,
) -> Option<BeamError> {
    let Some(item) = head else {
        return origin;
    };
    match origin {
        Some(BeamError::Raise(raise)) => {
            if raise.supplied_trace().is_some() {
                return Some(BeamError::Raise(raise));
            }
            match terms::make_list(ctx, &[item]) {
                Ok(trace) => Some(BeamError::Raise(
                    raise.with_supplied_trace(LiveReason::new(trace)),
                )),
                // A raise must never be masked by a `system_limit` from building its
                // own trace: the exception wins, the item is dropped.
                Err(_) => Some(BeamError::Raise(raise)),
            }
        }
        other => other,
    }
}
/// A term-building failure is a typed runtime error, never an Erlang exception, so it
/// travels as [`Flow::Fatal`] — the same funnel `mod.rs`'s `heap!` macro provides
/// (spelled as a helper here because a `macro_rules!` defined in the parent module
/// after `mod trace;` is not in this module's textual scope).
fn term<T>(built: Result<T>) -> EResult<T> {
    built.map_err(Flow::Fatal)
}

/// The live term behind one of a raise's opaque payload slots (`saved_args` /
/// `error_info`), read with the workspace convention
/// (`downcast_ref::<Eterm>().copied()`, the same one `flow_from_error` uses for
/// `live_reason`).
///
/// The word is the **raising call's own term**, read while the heap that built it
/// is still alive: the item is built inside the same statement, before the store is
/// rewritten at the next boundary.
fn live_slot(origin: &Option<BeamError>, slot: fn(&Raise) -> Option<&beam_common::LiveReason>) -> Option<Eterm> {
    match origin {
        Some(BeamError::Raise(raise)) => slot(raise)
            .and_then(|live| live.downcast_ref::<Eterm>())
            .copied(),
        _ => None,
    }
}

/// The **supplied trace term** of an `erlang:raise/3` raise, for the verbatim echo.
fn supplied_trace_term(raise: &Raise) -> Option<Eterm> {
    raise
        .supplied_trace()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::super::eval;
    use crate::config::RuntimeConfig;
    use crate::context::RuntimeContext;
    use crate::runtime::Runtime;

    /// Evaluates `expr` with `io:format/2` captured and returns exactly what it printed.
    ///
    /// The mirror of `mod.rs`'s `printed` helper (which is private to that module's test
    /// module): a heap-built trace cannot outlive the runtime, so the printed bytes are the
    /// only way to assert one.
    fn printed(expr: &str) -> String {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::BytesSink::new();
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        eval(&mut ctx, expr).unwrap_or_else(|error| panic!("{expr} failed: {error}"));
        drop(ctx);
        String::from_utf8_lossy(sink.contents()).into_owned()
    }

    /// The cooked trace of `expr`'s raise, as `~p` prints it, for a `catch`-shaped probe.
    fn caught(expr: &str) -> String {
        printed(&format!("io:format(\"~p~n\",[({expr})])"))
    }

    /// `try <expr> catch C:R:S -> ...` — the `try` form, which sees the trace directly.
    fn tried(expr: &str, body: &str) -> String {
        printed(&format!(
            "io:format(\"~p~n\",[try {expr} catch C:R:S -> {body} end])"
        ))
    }

    /// **One derived raise-site item, for every raise a call returns.**
    ///
    /// Each expectation is this walker's own shape (`docs`: the item is
    /// `{Callee, Function, A, Location}`), not the oracle's — the oracle's chain of
    /// `erl_eval`/`init` frames is deliberately not fabricated (see the module docs).
    #[test]
    fn every_call_derives_its_raise_site_item() {
        // A raise from a BIF declared in `bif.tab`: the item names **the BIF**, `A` is the
        // real evaluated argument list, and `Location` stays `[]` (the `{error_info, _}`
        // extension is `beam_engine`'s closed table, which a `-eval` raise does not reach).
        assert_eq!(
            caught("catch hd(1)"),
            "{'EXIT',{badarg,[{erlang,hd,[1],[]}]}}\n"
        );
        assert_eq!(
            caught("catch binary:at(<<1:1>>,0)"),
            "{'EXIT',{badarg,[{binary,at,[<<1:1>>,0],[]}]}}\n"
        );
        // An operator is an `erlang` BIF like any other (`'+'` is quoted in `~p`).
        assert_eq!(
            caught("catch 1+a"),
            "{'EXIT',{badarith,[{erlang,'+',[1,a],[]}]}}\n"
        );
        // A stdlib fast path: the failing function is the walker's call site.
        assert_eq!(
            caught("catch lists:max([])"),
            "{'EXIT',{function_clause,[{lists,max,[[]],[]}]}}\n"
        );
        // The `try` form sees the same item in the trace's head position.
        assert_eq!(
            tried("error(boom)", "{C,R,hd(S)}"),
            "{error,boom,{erlang,error,[boom],[]}}\n"
        );
    }

    /// **`erlang:error/2,3` substitute their own argument list and append the
    /// `error_info` extension** — the C's `non_bif_stacktrace`, reproduced from the
    /// `saved_args`/`error_info` payloads `beam_bifs` attaches.
    #[test]
    fn error_two_and_three_carry_their_own_payloads() {
        // `error/2`'s `A` slot is the argument list it was **given**, not the walker's
        // real `[boom, [1,2]]` call arguments.
        assert_eq!(
            caught("catch erlang:error(boom,[1,2])"),
            "{'EXIT',{boom,[{erlang,error,[1,2],[]}]}}\n"
        );
        // `error/3`'s options become the one-element `Location` list.
        assert_eq!(
            caught("catch erlang:error(boom,[1,2],[{error_info,#{module=>x}}])"),
            "{'EXIT',{boom,[{erlang,error,[1,2],[{error_info,#{module => x}}]}]}}\n"
        );
        // A non-list second argument is used **verbatim** as `A` (never validated and
        // never replaced by the real argument list).
        assert_eq!(
            caught("catch erlang:error(boom,notalist)"),
            "{'EXIT',{boom,[{erlang,error,notalist,[]}]}}\n"
        );
    }

    /// `exit/1`, `throw/1` and a caller-supplied trace are unchanged by the derived item.
    ///
    /// `exit`/`throw` render no trace by class rule (`catch` folds them by class, not by
    /// trace), and a supplied `erlang:raise/3` trace is echoed **verbatim** — the
    /// echo wins outright, so nothing is consed in front of it.
    #[test]
    fn exit_throw_and_a_supplied_trace_are_unaffected() {
        assert_eq!(caught("catch throw(ball)"), "ball\n");
        assert_eq!(caught("catch exit(bye)"), "{'EXIT',bye}\n");
        assert_eq!(
            caught("catch erlang:raise(error,r,[{m,f,1,[{file,\"m.erl\"},{line,7}]}])"),
            "{'EXIT',{r,[{m,f,1,[{file,\"m.erl\"},{line,7}]}]}}\n"
        );
        // The `try` form's third pattern sees the same supplied trace.
        assert_eq!(
            tried(
                "erlang:raise(error,r,[{m,f,1,[{file,\"m.erl\"},{line,7}]}])",
                "S"
            ),
            "[{m,f,1,[{file,\"m.erl\"},{line,7}]}]\n"
        );
    }

    /// A **bare `undef`** still yields the missing callee's item with its real argument
    /// list — the behaviour the item was originally derived for.
    #[test]
    fn an_undef_still_names_the_missing_callee() {
        assert_eq!(
            caught("catch nosuch:m(1)"),
            "{'EXIT',{undef,[{nosuch,m,[1],[]}]}}\n"
        );
        // A fun reference contributes the callee item only for an **external** `fun m:f/A`.
        assert_eq!(
            caught("catch (fun nosuch:f/1)(1)"),
            "{'EXIT',{undef,[{nosuch,f,[1],[]}]}}\n"
        );
    }

    /// **A derived item agrees with the oracle on everything the walker can derive**
    /// (`A`, and the `error_info` extension), and differs exactly where OTP names its
    /// own evaluator's frame.
    ///
    /// Both sides run the **same** probe program, so the comparison is like-for-like:
    /// the `A` slot carries `error/2,3`'s substituted argument list on both, the
    /// `{error_info, Map}` extension is the oracle's last `Location` entry too, and the
    /// item's identity (the `M`/`F` pair) is where the two part. The oracle's `Location`
    /// also carries its caller's `{file,_}`/`{line,_}` source position, which this walker
    /// never fabricates — pinned as a difference, not asserted equal (see the module
    /// docs' residuals).
    #[test]
    fn the_id_agrees_with_the_oracle_on_the_payloads_and_differs_on_the_identity() {
        let Some(()) = have_oracle() else {
            return;
        };

        // The `A` slot: `error/2,3`'s supplied argument list, verbatim, on both sides.
        for expr in [
            "erlang:error(boom,[1,2])",
            "erlang:error(boom,[1,2],[{error_info,#{module=>x}}])",
        ] {
            let probe = format!(
                "P = (catch {expr}), {{'EXIT',{{boom,[H|_]}}}} = P, \
                 io:format(\"~p~n\",[element(3,H)]), halt(0)."
            );
            let oracle = oracle_stdout(&probe).expect("the oracle ran");
            assert_eq!(oracle.trim(), "[1,2]", "the oracle's `A` slot: {expr}");
            assert_eq!(
                printed(&probe_without_halt(&probe)).trim(),
                "[1,2]",
                "our `A` slot must match the oracle's: {expr}"
            );
        }

        // The `error_info` extension: the oracle appends it to its own location, this
        // walker's whole `Location` *is* that one-element list — `lists:last/1` meets.
        let probe = "P = (catch erlang:error(boom,[1,2],[{error_info,#{module=>x}}])), \
                     {'EXIT',{boom,[H|_]}} = P, \
                     io:format(\"~p~n\",[lists:last(element(4,H))]), halt(0).";
        let expected = "{error_info,#{module => x}}";
        assert_eq!(
            oracle_stdout(probe).expect("the oracle ran").trim(),
            expected,
            "the oracle appends the `error_info` extension to its location"
        );
        assert_eq!(
            printed(&probe_without_halt(probe)).trim(),
            expected,
            "our `Location` is that same extension"
        );

        // The source position: the oracle's `Location` starts with its caller's
        // `{file, Line}`; ours is `[]` — the documented residual.
        let probe = "P = (catch erlang:error(boom,[1,2])), {'EXIT',{boom,[H|_]}} = P, \
                     io:format(\"~p~n\",[element(4,H)]), halt(0).";
        let oracle_location = oracle_stdout(probe).expect("the oracle ran");
        assert!(
            oracle_location.trim_start().starts_with("[{file,"),
            "the oracle's location carries a source position, got {oracle_location:?}"
        );
        assert_eq!(
            printed(&probe_without_halt(probe)).trim(),
            "[]",
            "this walker never fabricates a source position"
        );

        // The identity, pinned as a **known difference**: the oracle names its own
        // evaluator's frame, this walker names the call site it dispatched.
        let probe = "P = (catch erlang:error(boom,[1,2])), {'EXIT',{boom,[H|_]}} = P, \
                     io:format(\"~p~n\",[element(1,H)]), halt(0).";
        let oracle_module = oracle_stdout(probe).expect("the oracle ran");
        assert_eq!(
            oracle_module.trim(),
            "erl_eval",
            "the oracle's head item is its own evaluator's frame"
        );
        assert_eq!(
            printed(&probe_without_halt(probe)).trim(),
            "erlang",
            "this walker's head item names the call site it dispatched"
        );
        assert_ne!(oracle_module.trim(), "erlang");
    }

    /// Skips **loudly** when no system `erl` is available (`None` means "skip"), exactly as
    /// the bit-string suites' oracle probes do.
    fn have_oracle() -> Option<()> {
        match Command::new("erl")
            .args(["-noshell", "-eval", "halt(0)."])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => Some(()),
            Ok(status) => {
                eprintln!("SKIP: `erl` exited with {status} — live oracle cross-check skipped");
                None
            }
            Err(error) => {
                eprintln!("SKIP: no erl on PATH ({error}) — live oracle cross-check skipped");
                None
            }
        }
    }

    /// The same probe with the oracle's own `halt(0)` removed: this runtime has no
    /// `halt/1` in its expression grammar, so the driver must see a plain expression.
    fn probe_without_halt(probe: &str) -> String {
        probe
            .strip_suffix("halt(0).")
            .unwrap_or(probe)
            .trim_end_matches([' ', ','])
            .to_owned()
    }

    /// Runs `erl -noshell -eval <program>` with a **bounded** poll/kill and returns its
    /// stdout, or `None` when the probe could not be run (never a silent pass).
    fn oracle_stdout(program: &str) -> Option<String> {
        const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);
        let mut child = Command::new("erl")
            .args(["-noshell", "-eval", program])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + ORACLE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!(
                        "SKIPPING the trace oracle probe: `erl` did not exit within \
                         {ORACLE_TIMEOUT:?}"
                    );
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    eprintln!("SKIPPING the trace oracle probe: cannot poll `erl`: {error}");
                    return None;
                }
            }
        };
        let mut text = String::new();
        child.stdout.take()?.read_to_string(&mut text).ok()?;
        assert!(status.success(), "the oracle probe failed: {text:?}");
        Some(text)
    }
}
