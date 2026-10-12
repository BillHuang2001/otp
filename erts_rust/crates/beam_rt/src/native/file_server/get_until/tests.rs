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

//! Unit tests for the `{get_until, …}` io-device request ([`super`]).
//!
//! The protocol decoders ([`interpret`](super::interpret),
//! [`leftover_from`](super::leftover_from)) and the stream-encoding selection
//! ([`Runtime::open_encoding`]) are pinned directly; the request itself is driven through
//! the **real** dispatch arm ([`Runtime::serve_device_request`]) over a real temporary file
//! and the **real** `erl_scan:tokens/4` running on the engine.
//!
//! Every oracle value quoted below was verified against the system `erl` (OTP 29 /
//! erts-17.1); the exact command or one-liner is in the test's doc comment. A test whose
//! prerequisite is a real system Erlang skips **loudly** (`eprintln!` + early return) when
//! `erl_scan.beam` cannot be located, never silently passing.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::super::tests::{build, runtime};
use super::super::Device;
use super::*;

// -- fixtures -------------------------------------------------------------------------

/// [`runtime`] with a resolved system code path — `None` (the caller then skips **loudly**)
/// when `erl_scan.beam` cannot be found, because then the request's user function cannot be
/// run at all.
fn system_runtime() -> Option<Runtime> {
    let mut runtime = runtime();
    if runtime.find_module_file("erl_scan").is_none() {
        eprintln!("SKIP: no system Erlang (`erl_scan.beam` cannot be found)");
        return None;
    }
    Some(runtime)
}

/// A `{get_until, Enc, Prompt, erl_scan, tokens, [Pos0, Options]}` request — byte-for-byte
/// the request `io:scan_erl_exprs/4` sends (`lib/stdlib/src/io.erl:1423`), with `Pos0 =
/// {Line, 1}` and empty `Options`.
fn tokens_request(runtime: &mut Runtime, enc: &str, line: i64) -> Eterm {
    build(runtime, |ctx| {
        let get_until = ctx.atom("get_until").unwrap();
        let encoding = ctx.atom(enc).unwrap();
        let module = ctx.atom("erl_scan").unwrap();
        let function = ctx.atom("tokens").unwrap();
        let location =
            terms::make_tuple(ctx, &[make_small(line), make_small(1)]).unwrap();
        let options = terms::make_list(ctx, &[]).unwrap();
        let xtra_args = terms::make_list(ctx, &[location, options]).unwrap();
        terms::make_tuple(
            ctx,
            &[get_until, encoding, NIL, module, function, xtra_args],
        )
        .unwrap()
    })
}

/// A `{get_until, Enc, Prompt, Mod, Func, XtraArgs}` request for any user function — the
/// general form of [`tokens_request`], so a test can drive a continuation that is neither
/// the scanner nor protocol-conforming.
fn request(
    runtime: &mut Runtime,
    enc: &str,
    module: &str,
    function: &str,
    xtra_args: &[Eterm],
) -> Eterm {
    build(runtime, |ctx| {
        let get_until = ctx.atom("get_until").unwrap();
        let encoding = ctx.atom(enc).unwrap();
        let module = ctx.atom(module).unwrap();
        let function = ctx.atom(function).unwrap();
        let xtra_args = terms::make_list(ctx, xtra_args).unwrap();
        terms::make_tuple(
            ctx,
            &[get_until, encoding, NIL, module, function, xtra_args],
        )
        .unwrap()
    })
}

/// Registers `file` as a fresh io-device — the shape [`super::super::Runtime::reply_open`]
/// builds — and returns the pid it is keyed by.
///
/// The devices map is keyed by pid and only ever read by pid, so the request path is
/// exercised without a process-table entry (which `Runtime` exposes no mutable accessor
/// for); the pid is deliberately outside the table's number range.
fn open_device(runtime: &mut Runtime, file: File, encoding: Encoding, binary: bool) -> ProcessId {
    let pid = ProcessId::new(900_000, 1);
    runtime.file_services.devices.insert(
        pid,
        Device {
            file,
            binary,
            encoding,
            buf: Vec::new(),
        },
    );
    pid
}

/// Runs `f` over a unique scratch file under `std::env::temp_dir()` holding `contents`,
/// removing it afterwards.
fn with_scratch<R>(name: &str, contents: &str, f: impl FnOnce(&Path) -> R) -> R {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "beam_rt_get_until_{}_{name}.erl",
        std::process::id()
    ));
    let mut file = File::create(&path).expect("a scratch file");
    file.write_all(contents.as_bytes())
        .expect("writing the scratch file");
    drop(file);
    let result = f(&path);
    let _ = std::fs::remove_file(&path);
    result
}

/// The `~w` rendering of `term`, without the printer's trailing newline — the oracle's own
/// `io_lib:format("~w", [Term])`, which is what the values below are compared against.
fn render(runtime: &mut Runtime, term: Eterm) -> String {
    let rendered = runtime.format_term_w(term, None).expect("a rendered term");
    rendered.strip_suffix('\n').map(str::to_owned).unwrap_or(rendered)
}

/// The reason atom of a typed capability gap.
fn gap_reason(error: BeamError) -> String {
    let raise = error
        .into_raise()
        .expect("a capability gap is an explicit raise");
    raise
        .reason
        .as_atom()
        .expect("an atom reason")
        .to_string()
}

/// The stashed device buffer (`State#state.buf`) of `pid`.
fn device_buffer(runtime: &Runtime, pid: ProcessId) -> Vec<u8> {
    runtime.file_services.devices[&pid].buf.clone()
}

// -- the protocol decoders ------------------------------------------------------------

/// `io_lib:get_until/4`'s two answer shapes: `{more, Cont}` and `{done, Result, Buf}`.
///
/// The `Buf` of a `{done, …}` is decoded through [`leftover_from`], so
/// `{done, R, <<>>}`/`{done, R, []}`/`{done, R, eof}` all carry
/// [`Leftover::None`].
#[test]
fn interpret_decodes_more_and_done() {
    let mut runtime = runtime();
    let cont = build(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(1)]).unwrap());
    let more = build(&mut runtime, |ctx| {
        let tag = ctx.atom("more").unwrap();
        terms::make_tuple(ctx, &[tag, cont]).unwrap()
    });
    assert_eq!(
        interpret(&runtime, more).unwrap(),
        Some(Applied::More(cont)),
        "`{{more, Cont}}` carries the continuation unchanged"
    );

    let done = build(&mut runtime, |ctx| {
        let tag = ctx.atom("done").unwrap();
        let reply = make_small(7);
        let buf = terms::make_heap_binary(ctx, b"ab").unwrap();
        terms::make_tuple(ctx, &[tag, reply, buf]).unwrap()
    });
    assert_eq!(
        interpret(&runtime, done).unwrap(),
        Some(Applied::Done {
            reply: make_small(7),
            leftover: Leftover::Binary(b"ab".to_vec()),
        })
    );

    // `{done, R, eof}` — `erl_scan:tokens/4`'s end-of-file answer (`Buf = eof`).
    let done_eof = build(&mut runtime, |ctx| {
        let tag = ctx.atom("done").unwrap();
        let eof = ctx.atom("eof").unwrap();
        terms::make_tuple(ctx, &[tag, make_small(7), eof]).unwrap()
    });
    assert_eq!(
        interpret(&runtime, done_eof).unwrap(),
        Some(Applied::Done {
            reply: make_small(7),
            leftover: Leftover::None,
        })
    );
}

/// Anything that is not `{more, Cont}` / `{done, Result, Buf}` is the `case_clause` shape —
/// `Ok(None)` here, which [`EngineContinuation::apply`] turns into the typed capability gap.
#[test]
fn interpret_answers_none_for_a_non_protocol_value() {
    let mut runtime = runtime();
    // Not a tuple at all.
    assert_eq!(interpret(&runtime, make_small(1)).unwrap(), None);
    // A 0-element tuple (`{}`) has no head to read a tag from, and a 1-element tuple
    // cannot match either protocol arity — both are the `case_clause`, never an index.
    let empty = build(&mut runtime, |ctx| terms::make_tuple(ctx, &[]).unwrap());
    assert_eq!(interpret(&runtime, empty).unwrap(), None);
    let one = build(&mut runtime, |ctx| {
        let tag = ctx.atom("more").unwrap();
        terms::make_tuple(ctx, &[tag]).unwrap()
    });
    assert_eq!(interpret(&runtime, one).unwrap(), None);
    // Right tag, wrong arity in both directions.
    let done_two = build(&mut runtime, |ctx| {
        let tag = ctx.atom("done").unwrap();
        terms::make_tuple(ctx, &[tag, make_small(1)]).unwrap()
    });
    assert_eq!(interpret(&runtime, done_two).unwrap(), None);
    let more_three = build(&mut runtime, |ctx| {
        let tag = ctx.atom("more").unwrap();
        terms::make_tuple(ctx, &[tag, make_small(1), make_small(2)]).unwrap()
    });
    assert_eq!(interpret(&runtime, more_three).unwrap(), None);
    // An unrelated tag.
    let other = build(&mut runtime, |ctx| {
        let tag = ctx.atom("foo").unwrap();
        terms::make_tuple(ctx, &[tag, make_small(1), make_small(2)]).unwrap()
    });
    assert_eq!(interpret(&runtime, other).unwrap(), None);
}

/// `cast_binary/1`'s three representable shapes (`file_io_server.erl:805-812`) plus the
/// char-list form: a binary, a char list of code points, and nothing at all.
#[test]
fn leftover_from_maps_the_representable_shapes() {
    let mut runtime = runtime();

    let empty_binary = build(&mut runtime, |ctx| terms::make_heap_binary(ctx, b"").unwrap());
    assert_eq!(leftover_from(&runtime, empty_binary).unwrap(), Leftover::None);
    assert_eq!(leftover_from(&runtime, NIL).unwrap(), Leftover::None);
    let eof = build(&mut runtime, |ctx| ctx.atom("eof").unwrap());
    assert_eq!(leftover_from(&runtime, eof).unwrap(), Leftover::None);

    let binary = build(&mut runtime, |ctx| terms::make_heap_binary(ctx, b"\x01\x02").unwrap());
    assert_eq!(
        leftover_from(&runtime, binary).unwrap(),
        Leftover::Binary(vec![1, 2])
    );

    // `erl_scan:tokens/4` answers a char list when its data was a char list — the
    // `{done,{ok,…},[114,101,115,116]}` shape the oracle gives for `1+2.\nrest`.
    let chars = build(&mut runtime, |ctx| {
        terms::make_list(ctx, &[make_small(97), make_small(98)]).unwrap()
    });
    assert_eq!(
        leftover_from(&runtime, chars).unwrap(),
        Leftover::Chars(vec![97, 98])
    );
}

/// A `Buf` this layer cannot represent is reported, never truncated: a deep iolist (the
/// oracle's `list_to_binary/1` would flatten it), a non-charlist, a char list holding a
/// non-integer, and a negative code point.
#[test]
fn leftover_from_reports_an_unrepresentable_leftover() {
    let mut runtime = runtime();

    let deep_iolist = build(&mut runtime, |ctx| {
        let binary = terms::make_heap_binary(ctx, b"ab").unwrap();
        let tail = terms::make_list(ctx, &[make_small(99)]).unwrap();
        terms::make_list(ctx, &[binary, tail]).unwrap()
    });
    assert!(leftover_from(&runtime, deep_iolist).is_err());

    let tuple = build(&mut runtime, |ctx| {
        let tag = ctx.atom("a").unwrap();
        terms::make_tuple(ctx, &[tag]).unwrap()
    });
    assert!(leftover_from(&runtime, tuple).is_err());

    let mixed = build(&mut runtime, |ctx| {
        let binary = terms::make_heap_binary(ctx, b"ab").unwrap();
        terms::make_list(ctx, &[binary]).unwrap()
    });
    assert!(leftover_from(&runtime, mixed).is_err());

    let negative = build(&mut runtime, |ctx| {
        terms::make_list(ctx, &[make_small(-1)]).unwrap()
    });
    assert!(leftover_from(&runtime, negative).is_err());

    // A non-byte-aligned bitstring is not `is_binary`, so it cannot be a device buffer
    // either — the oracle's `cast_binary/1` reaches `list_to_binary/1` and raises `badarg`.
    let bitstring = runtime.eval("<<1:1>>").expect("a bitstring");
    assert!(leftover_from(&runtime, bitstring).is_err());
}

/// `file_io_server:parse_options/1`'s stream encoding: `unicode`/`utf8` — bare or as the
/// `{encoding, …}` option — selects [`Encoding::Unicode`]; everything else, including the
/// unmodelled `{utf16,_}`/`{utf32,_}` tuple encodings, is `latin1`.
#[test]
fn open_encoding_selects_the_stream_encoding() {
    let mut runtime = runtime();
    let mode = |runtime: &mut Runtime, names: &[&str]| -> Eterm {
        let names: Vec<String> = names.iter().map(|name| (*name).to_string()).collect();
        build(runtime, |ctx| {
            let mut elements = Vec::with_capacity(names.len());
            for name in &names {
                elements.push(ctx.atom(name).unwrap());
            }
            terms::make_list(ctx, &elements).unwrap()
        })
    };

    assert_eq!(runtime.open_encoding(NIL), Encoding::Latin1);
    let read = mode(&mut runtime, &["read"]);
    assert_eq!(runtime.open_encoding(read), Encoding::Latin1);
    let read_binary = mode(&mut runtime, &["read", "binary"]);
    assert_eq!(runtime.open_encoding(read_binary), Encoding::Latin1);
    let unicode = mode(&mut runtime, &["read", "unicode"]);
    assert_eq!(runtime.open_encoding(unicode), Encoding::Unicode);
    // The oracle's `file:open/2` accepts `utf8` as a synonym of `unicode`.
    let utf8 = mode(&mut runtime, &["read", "utf8"]);
    assert_eq!(runtime.open_encoding(utf8), Encoding::Unicode);
    // `{utf16, big}` has no `beam_io` counterpart, so it reads as latin1 (documented
    // divergence, shared with `{get_chars, …}`).
    let utf16 = build(&mut runtime, |ctx| {
        let utf16 = ctx.atom("utf16").unwrap();
        let big = ctx.atom("big").unwrap();
        let pair = terms::make_tuple(ctx, &[utf16, big]).unwrap();
        terms::make_list(ctx, &[pair]).unwrap()
    });
    assert_eq!(runtime.open_encoding(utf16), Encoding::Latin1);

    // An `{encoding, …}` pair overrides the streaming default.
    for encoding in ["utf8", "unicode"] {
        let option = build(&mut runtime, |ctx| {
            let key = ctx.atom("encoding").unwrap();
            let value = ctx.atom(encoding).unwrap();
            let pair = terms::make_tuple(ctx, &[key, value]).unwrap();
            let read = ctx.atom("read").unwrap();
            terms::make_list(ctx, &[read, pair]).unwrap()
        });
        assert_eq!(runtime.open_encoding(option), Encoding::Unicode, "{encoding}");
    }
    let latin1_option = build(&mut runtime, |ctx| {
        let key = ctx.atom("encoding").unwrap();
        let value = ctx.atom("latin1").unwrap();
        let pair = terms::make_tuple(ctx, &[key, value]).unwrap();
        let read = ctx.atom("read").unwrap();
        terms::make_list(ctx, &[read, pair]).unwrap()
    });
    assert_eq!(runtime.open_encoding(latin1_option), Encoding::Latin1);
}

// -- the request's rejections ---------------------------------------------------------

/// A **request** encoding that is neither `latin1` nor `unicode` is the typed capability
/// gap: `beam_io` models only the two byte-oriented encodings, and the tuple encodings
/// would need `unicode:characters_to_binary/3`.
#[test]
fn serve_rejects_an_unknown_request_encoding() {
    let mut runtime = runtime();
    for enc in ["utf16", "latin-1"] {
        let request = build(&mut runtime, |ctx| {
            let get_until = ctx.atom("get_until").unwrap();
            let encoding = ctx.atom(enc).unwrap();
            let module = ctx.atom("erl_scan").unwrap();
            let function = ctx.atom("tokens").unwrap();
            let xtra_args = terms::make_list(ctx, &[]).unwrap();
            terms::make_tuple(ctx, &[get_until, encoding, NIL, module, function, xtra_args])
                .unwrap()
        });
        let error = runtime
            .serve_device_request(ProcessId::new(900_000, 1), request)
            .expect_err("an unmodelled request encoding");
        assert!(gap_reason(error).contains("request encoding"), "{enc}");
    }
    // A non-atom `Enc` cannot name an encoding at all.
    let request = build(&mut runtime, |ctx| {
        let get_until = ctx.atom("get_until").unwrap();
        let module = ctx.atom("erl_scan").unwrap();
        let function = ctx.atom("tokens").unwrap();
        let xtra_args = terms::make_list(ctx, &[]).unwrap();
        terms::make_tuple(
            ctx,
            &[get_until, make_small(16), NIL, module, function, xtra_args],
        )
        .unwrap()
    });
    assert!(runtime
        .serve_device_request(ProcessId::new(900_000, 1), request)
        .is_err());
}

/// A request for a pid no device is registered under is the "closed io device" gap — the
/// oracle's device process no longer exists, so there is nothing to answer from.
#[test]
fn serve_reports_a_request_for_a_closed_device() {
    let mut runtime = runtime();
    let request = tokens_request(&mut runtime, "latin1", 1);
    let error = runtime
        .serve_device_request(ProcessId::new(900_000, 1), request)
        .expect_err("no device is registered for that pid");
    let reason = gap_reason(error);
    assert!(reason.starts_with("unsupported: "), "{reason}");
    assert!(reason.contains("closed io device"), "{reason}");
}

/// `get_chars_apply/7` boxes a raising user function as `{stop, Reason, {error, Func},
/// State}` (`file_io_server.erl:681-682`) and `server_loop/1` replies **then** dies. The
/// reply here is the value `{error, Func}` with the retire flag.
#[test]
fn serve_boxes_a_continuation_raise_as_error_func_and_retires_the_device() {
    let mut runtime = runtime();
    // A `Func` that is not an atom cannot be named; `err_func/3` reports `badarg`.
    let non_atom_func = build(&mut runtime, |ctx| {
        let get_until = ctx.atom("get_until").unwrap();
        let encoding = ctx.atom("latin1").unwrap();
        let module = ctx.atom("erl_scan").unwrap();
        let xtra_args = terms::make_list(ctx, &[]).unwrap();
        terms::make_tuple(
            ctx,
            &[get_until, encoding, NIL, module, make_small(42), xtra_args],
        )
        .unwrap()
    });
    let (reply, retire) = runtime
        .serve_device_request(ProcessId::new(900_000, 1), non_atom_func)
        .expect("the oracle replies `{error, Func}` rather than propagating the reason");
    assert!(retire, "the device dies after the reply");
    let fields = tuple_fields(reply).expect("a 2-tuple");
    assert_eq!(runtime.atom_name(fields[0]).as_deref(), Some("error"));
    assert_eq!(runtime.atom_name(fields[1]).as_deref(), Some("badarg"));

    // `apply(Mod, Func, [Cont, Chars | XtraArgs])` with a non-list `XtraArgs` is `badarg`,
    // and `Func` here *is* an atom, so it is named.
    let non_list_args = build(&mut runtime, |ctx| {
        let get_until = ctx.atom("get_until").unwrap();
        let encoding = ctx.atom("latin1").unwrap();
        let module = ctx.atom("erl_scan").unwrap();
        let function = ctx.atom("tokens").unwrap();
        terms::make_tuple(
            ctx,
            &[get_until, encoding, NIL, module, function, make_small(42)],
        )
        .unwrap()
    });
    let (reply, retire) = runtime
        .serve_device_request(ProcessId::new(900_000, 1), non_list_args)
        .expect("the oracle replies `{error, Func}`");
    assert!(retire, "the device dies after the reply");
    assert_eq!(
        render(&mut runtime, reply),
        "{error,tokens}",
        "the reply is the value `{{error, Func}}`"
    );
}

// -- the raise/gap classification ------------------------------------------------------

/// A **typed capability gap** of this layer is not a raise of the user function: it travels
/// out as [`Err`] and the device stays registered, still answering requests.
///
/// The continuation here is `lists:flatten/2` applied as
/// `lists:flatten(Cont, Chars)`: it is real Erlang code (`lists.erl:1320`, unlike the
/// `erlang:nif_error(undef)` stub `lists:reverse/2` is) whose value is the charlist itself —
/// i.e. neither `{more, Cont}` nor `{done, Result, Buf}`, which is the gap [`interpret`]
/// reports (`Ok(None)`).
///
/// The oracle, for comparison, reaches `io_lib:get_until/4`'s own `case_clause`, which
/// `file_io_server`'s `catch` boxes as `{error, Func}` and then dies
/// (`file_io_server.erl:681-682`; probed on OTP 29.1.1 / erts-17.1 with
/// `1+2.\nrest` as `gap_probe.erl`):
///
/// ```text
/// 1> {ok,D} = file:open("gap_probe.erl",[read]),
///    io:request(D, {get_until,latin1,"",lists,flatten,[]}).
/// {error,flatten}
/// 2> io:request(D, {get_until,latin1,"",lists,flatten,[]}).
/// {error,terminated}
/// ```
///
/// This runtime reports the honest typed gap instead of fabricating a user-function raise
/// (a capability gap is uncatchable and must never be dressed up as `{error, Func}`), and
/// the device is **not** retired.
#[test]
fn a_typed_capability_gap_neither_retires_the_device_nor_reports_error_func() {
    let Some(mut runtime) = system_runtime() else {
        return;
    };
    with_scratch("gap", "1+2.\nrest", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let request = request(&mut runtime, "latin1", "lists", "flatten", &[]);
        let failure = runtime
            .serve_device_request(pid, request)
            .expect_err("a non-protocol return value is the typed gap");
        let reason = crate::error::unsupported_reason(&failure)
            .expect("the gap convention is the single interpreter of `unsupported:`");
        assert!(
            reason.contains("neither `{more, Cont}` nor `{done, Result, Buf}`"),
            "{reason}"
        );
        assert!(
            runtime.file_services.devices.contains_key(&pid),
            "a typed gap must not retire the device"
        );

        // ... and the device really is still serving: the same request family answers, and
        // the scanner sees the already-consumed file as end of file.
        let second = tokens_request(&mut runtime, "latin1", 1);
        let (reply, retire) = runtime
            .serve_device_request(pid, second)
            .expect("the device survived the gap");
        assert!(!retire);
        assert_eq!(render(&mut runtime, reply), "{eof,{1,1}}");
    });
}

/// A **genuine raise of the user function** keeps the oracle's behaviour: the reply is the
/// value `{error, Func}` and the device is retired — `file_io_server`'s `{'EXIT', Reason}`
/// arm through `get_chars_apply/7` (`file_io_server.erl:681-682`) plus `server_loop/1`'s
/// `exit(Reason)`.
///
/// The continuation is `lists:nth/2` applied as `lists:nth(Cont, Chars)`: with the empty
/// list as `Cont` no clause matches, so the callee raises `function_clause` — exactly the
/// kind of raise the classification must not mistake for a capability gap. The oracle, probed
/// the same way as above:
///
/// ```text
/// 1> {ok,E} = file:open("gap_probe.erl",[read]),
///    io:request(E, {get_until,latin1,"",lists,nth,[]}).
/// {error,nth}
/// 2> io:request(E, {get_until,latin1,"",erl_scan,tokens,[{1,1},[]]}).
/// {error,terminated}
/// ```
#[test]
fn a_genuine_continuation_raise_retires_and_reports_error_func() {
    let Some(mut runtime) = system_runtime() else {
        return;
    };
    with_scratch("raise", "1+2.\nrest", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let request = request(&mut runtime, "latin1", "lists", "nth", &[]);
        let (reply, retire) = runtime
            .serve_device_request(pid, request)
            .expect("the oracle replies `{error, Func}` rather than propagating the reason");
        assert!(retire, "a raising user function kills the device");
        assert_eq!(render(&mut runtime, reply), "{error,nth}");
    });
}

/// [`EngineContinuation::classify`] is the single decision point: the workspace
/// `unsupported:` convention (via [`crate::error::unsupported_reason`]) separates a typed
/// capability gap from a genuine raise of the user function.
#[test]
fn classify_marks_only_a_genuine_raise() {
    let mut runtime = runtime();
    let mut continuation = EngineContinuation {
        runtime: &mut runtime,
        module: None,
        module_name: "lists".to_string(),
        function: "reverse".to_string(),
        arity: 2,
        xtra_args: Vec::new(),
        raised: false,
    };

    let gap = error::unsupported("file_server: a typed gap");
    let gap = continuation.classify(gap);
    assert!(
        crate::error::unsupported_reason(&gap).is_some(),
        "a gap stays a gap"
    );
    assert!(!continuation.raised, "a gap is not a raise of the user function");

    // `BeamError::BadArg` is what a BIF the user function called reports; nothing about it
    // is an `unsupported:` reason, so it is a genuine raise.
    continuation.classify(BeamError::BadArg);
    assert!(continuation.raised, "a user function's badarg is a raise");
}

// -- the request, end to end (the device loop + the real `erl_scan:tokens/4`) -----------

/// The oracle, with `printf '1+2.\nrest' > a.erl`:
///
/// ```text
/// 1> {ok,D} = file:open("a.erl",[read]), io:scan_erl_exprs(D,"",{1,1},[]).
/// {ok,[{integer,{1,1},1},{'+',{1,2}},{integer,{1,3},2},{dot,{1,4}}],{2,1}}
/// ```
///
/// The unconsumed `"rest"` is stashed in the device (`State#state.buf`), ready for the next
/// request.
#[test]
fn get_until_serves_a_complete_form() {
    let Some(mut runtime) = system_runtime() else {
        return;
    };
    with_scratch("complete", "1+2.\nrest", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let request = tokens_request(&mut runtime, "latin1", 1);
        let (reply, retire) = runtime
            .serve_device_request(pid, request)
            .expect("a served request");
        assert!(!retire, "a served request never retires the device");
        assert_eq!(
            render(&mut runtime, reply),
            "{ok,[{integer,{1,1},1},{'+',{1,2}},{integer,{1,3},2},{dot,{1,4}}],{2,1}}"
        );
        assert_eq!(device_buffer(&runtime, pid), b"rest");
    });
}

/// `io:scan_erl_exprs/4` is the request `io` itself sends: `Enc = unicode` with a
/// `latin1` **stream** (`file:open(P,[read])`), so `file_io_server`'s general
/// `get_chars_apply/7` clause runs (`InEnc = latin1`, `OutEnc = unicode`). The reply is
/// identical to the latin1 request's.
#[test]
fn get_until_serves_the_unicode_request_io_sends() {
    let Some(mut runtime) = system_runtime() else {
        return;
    };
    with_scratch("unicode", "1+2.\nrest", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let request = tokens_request(&mut runtime, "unicode", 1);
        let (reply, retire) = runtime
            .serve_device_request(pid, request)
            .expect("a served request");
        assert!(!retire);
        assert_eq!(
            render(&mut runtime, reply),
            "{ok,[{integer,{1,1},1},{'+',{1,2}},{integer,{1,3},2},{dot,{1,4}}],{2,1}}"
        );
        assert_eq!(
            device_buffer(&runtime, pid),
            b"rest",
            "the leftover is re-encoded into the device's latin1 stream on the way back"
        );
    });
}

/// A `binary`-read-mode device reads `8 KiB` at a time (`?READ_SIZE_BINARY`) and hands the
/// continuation a binary, so `io_lib:get_until/4` fixes `IsDataBinary = true` — the reply
/// is unchanged, because `erl_scan:tokens/4` answers a tuple and the cast only rewrites a
/// *list* reply.
#[test]
fn get_until_serves_a_binary_mode_device() {
    let Some(mut runtime) = system_runtime() else {
        return;
    };
    with_scratch("binary_mode", "1+2.\nrest", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            true,
        );
        let request = tokens_request(&mut runtime, "latin1", 1);
        let (reply, retire) = runtime
            .serve_device_request(pid, request)
            .expect("a served request");
        assert!(!retire);
        assert_eq!(
            render(&mut runtime, reply),
            "{ok,[{integer,{1,1},1},{'+',{1,2}},{integer,{1,3},2},{dot,{1,4}}],{2,1}}"
        );
        assert_eq!(device_buffer(&runtime, pid), b"rest");
    });
}

/// `printf '   \n' > b.erl`:
///
/// ```text
/// 1> {ok,D} = file:open("b.erl",[read]), io:scan_erl_exprs(D,"",{1,1},[]).
/// {eof,{2,1}}
/// ```
///
/// Two applications: the first chunk is all whitespace, so `erl_scan:tokens/4` answers
/// `{more, …}`; the end of file is then applied as the atom `eof`, which closes the form.
#[test]
fn get_until_finds_eof_in_a_whitespace_only_file() {
    let Some(mut runtime) = system_runtime() else {
        return;
    };
    with_scratch("eof", "   \n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let request = tokens_request(&mut runtime, "latin1", 1);
        let (reply, retire) = runtime
            .serve_device_request(pid, request)
            .expect("a served request");
        assert!(!retire);
        assert_eq!(render(&mut runtime, reply), "{eof,{2,1}}");
        assert!(
            device_buffer(&runtime, pid).is_empty(),
            "`{{done, _, eof}}` carries no leftover"
        );
    });
}

/// `printf '1.0er.\n' > d.erl`:
///
/// ```text
/// 1> {ok,D} = file:open("d.erl",[read]),
///    io:scan_erl_exprs(D,"",{1,1},[]), io:scan_erl_exprs(D,"",{1,1},[]).
/// {error,{{1,1},erl_scan,{illegal,float}},{1,5}}
/// {ok,[{atom,{1,1},r},{dot,{1,2}}],{2,1}}
/// ```
///
/// The scanner's own error tuple is the reply, verbatim — the device neither wraps nor
/// rewrites it. The scanner stopped **inside** the form, so what it did not consume
/// (`erl_scan:tokens([], "1.0er.\n", {1,1}, [])` ⇒ `{done, …, [114,46,10]}`, i.e. `"r.\n"`)
/// stays in the device and is what the second request scans.
#[test]
fn get_until_answers_the_scanners_error_tuple() {
    let Some(mut runtime) = system_runtime() else {
        return;
    };
    with_scratch("error", "1.0er.\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let request = tokens_request(&mut runtime, "latin1", 1);
        let (reply, retire) = runtime
            .serve_device_request(pid, request)
            .expect("a served request");
        assert!(!retire, "a scanner error is not a raise");
        assert_eq!(
            render(&mut runtime, reply),
            "{error,{{1,1},erl_scan,{illegal,float}},{1,5}}"
        );
        assert_eq!(
            device_buffer(&runtime, pid),
            b"r.\n",
            "the scanner stopped inside the form, so the tail is the device buffer"
        );

        let second = tokens_request(&mut runtime, "latin1", 1);
        let (reply, _) = runtime
            .serve_device_request(pid, second)
            .expect("the tail is served from the stashed buffer");
        assert_eq!(render(&mut runtime, reply), "{ok,[{atom,{1,1},r},{dot,{1,2}}],{2,1}}");
        assert!(device_buffer(&runtime, pid).is_empty());
    });
}

/// The **second** request over the same device: the oracle's
///
/// ```text
/// 1> {ok,D} = file:open("a.erl",[read]),
///    io:scan_erl_exprs(D,"",{1,1},[]), io:scan_erl_exprs(D,"",{1,1},[]).
/// {ok,[{atom,{1,1},rest}],{1,5}}
/// ```
///
/// proves the stashed leftover is fed to the scanner **before** any fresh read: the
/// `"rest"` the first request did not consume is what the second one scans (the file itself
/// is already at end of file).
#[test]
fn get_until_feeds_the_stashed_leftover_to_the_next_request() {
    let Some(mut runtime) = system_runtime() else {
        return;
    };
    with_scratch("leftover", "1+2.\nrest", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let first = tokens_request(&mut runtime, "latin1", 1);
        let (reply, _) = runtime
            .serve_device_request(pid, first)
            .expect("the first request is served");
        assert_eq!(
            render(&mut runtime, reply),
            "{ok,[{integer,{1,1},1},{'+',{1,2}},{integer,{1,3},2},{dot,{1,4}}],{2,1}}"
        );
        assert_eq!(device_buffer(&runtime, pid), b"rest");

        let second = tokens_request(&mut runtime, "latin1", 1);
        let (reply, retire) = runtime
            .serve_device_request(pid, second)
            .expect("the second request is served from the stashed buffer");
        assert!(!retire);
        assert_eq!(render(&mut runtime, reply), "{ok,[{atom,{1,1},rest}],{1,5}}");
        assert!(
            device_buffer(&runtime, pid).is_empty(),
            "the second request consumed the leftover and read to the end of the file"
        );
    });
}
