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

//! Integration tests: the terminal printer renders a **port** as the oracle's
//! `#Port<Channel.Num>` and classifies every term class it cannot render
//! byte-faithfully as a **typed** gap.
//!
//! `src/print.rs`'s `render_chunk` renders an internal port as `#Port<0.N>` — the
//! local node's channel number (`LOCAL_CHANNEL_NO`) and the port-table number, the
//! shape `src/port.rs`'s `port_to_list/1` and `src/term_conv.rs`'s `port_text` also
//! produce. These tests pin that rendering (`~w`/`~p`/`~W`/`~P` and the `io_lib`
//! entry point), the nesting of a port inside a list/tuple/map, and the
//! **external** identifier gap.
//!
//! # Why an external identifier is a typed gap and not a rendering
//!
//! `erl_printf_term.c:508-530` renders an external pid/port/ref through
//! `pid_channel_no`/`port_channel_no`/`ref_channel_no`, and for a term naming a
//! *foreign* node those are `external_channel_no(x)` =
//! `dist_entry_channel_no(external_dist_entry(x))` = **`atom_val(x->sysname)`**
//! (`erl_node_container_utils.h:109-116`) — the *atom index of the node-name atom
//! in the running VM's atom table*. That index is runtime state: it depends on when
//! and in which order the VM interned that atom. Measured on the OTP 29.1.1 /
//! erts-17.1 oracle, the same `V4_PORT_EXT` image naming `a@b` printed
//! `#Port<10643.7>` in one run and `#Port<10639.7>` in the next, and three images
//! decoded in one run printed channels `10639`/`10640`/`10641` for the nodes
//! `a@b`/`c@d`/`z@w` — consecutive atom indices. The frozen external-identifier
//! term (`beam_term::thing::write_external_{pid,port,ref}`) stores the node *atom*,
//! never a channel number, and the crate's own `AtomTable` index is a different
//! number again — so **no byte-faithful text exists**. A guessed channel (e.g. the
//! local `0`) would be a *silently wrong* result, so each is the typed
//! `unsupported_bif` (`BeamError::UnsupportedBif`), never a fabricated `#Port<…>`.
//!
//! Every golden below was produced by
//!
//! ```text
//! erl -noshell -eval 'io:format(Fmt, Args), halt().'
//! ```
//!
//! on the system oracle, and the re-checkable cases are re-run live (a missing
//! `erl` skips loudly rather than passing vacuously).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_common::BeamError;
use beam_process::{PortId, ProcessId};
use beam_term::map::{map_words_capacity, write_map};
use beam_term::prelude::*;

use std::process::Command;

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// An atom table seeded with the atoms the tests need: the node names of the
/// external-identifier wire images (so decoding one needs no `AtomInterner`), plus
/// the printable/binary fixtures.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "hello",
        "k",
        "a@b",
        "remote@host",
        // The `erts_internal:term_type/1` names the shape assertions read.
        "external_port",
        "external_pid",
        "external_reference",
    ]);
    atoms
}

/// A flat proper list of the byte-sized integers of `bytes` (an Erlang string).
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let mut elems = Vec::with_capacity(bytes.len());
    for &b in bytes {
        elems.push(ctx.int(i64::from(b)).expect("a byte fits a small int"));
    }
    ctx.list(&elems).expect("a short list fits")
}

/// A binary built through the registered `erlang:list_to_binary/1`.
fn binary(ctx: &mut BifContext<'_>, reg: &BifRegistry, bytes: &[u8]) -> Eterm {
    let list = byte_list(ctx, bytes);
    reg.call("erlang", "list_to_binary", 1, ctx, &[list])
        .expect("list_to_binary fits")
}

/// A flat map with the given pairs (`beam_term::map::write_map` picks the flat
/// representation at ≤32 pairs, which is what the oracle does).
fn flat_map_of(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let cap = map_words_capacity(pairs.len());
    // SAFETY: `ctx.heap()` hands out `cap` contiguous, exclusively-owned words from a
    // `beam_alloc::Heap` that never reallocates and outlives every term derived from
    // it, which is exactly `write_map`'s reservation contract.
    let hp = unsafe { ctx.heap().alloc_words(cap) }.expect("the test heap has room");
    // SAFETY: `hp` points at `cap == map_words_capacity(pairs.len())` exclusive words.
    unsafe { write_map(hp, pairs, ctx.atoms()) }.expect("a well-formed map")
}

/// Runs `io:format(Fmt, Args)` through a `BytesSink`; returns `(bytes, result)`.
fn run_io_args(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Vec<Eterm>,
) -> (Vec<u8>, Result<(), BeamError>) {
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let result;
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, fmt);
        let args = build(&mut ctx, reg);
        let args = ctx.list(&args).expect("a short arg list fits");
        result = reg
            .call("io", "format", 2, &mut ctx, &[format, args])
            .map(|_| ());
    }
    (sink.contents().to_vec(), result)
}

/// Runs `io:format(Fmt, [Arg])` through a `BytesSink`; returns `(bytes, result)`.
fn run_io(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Eterm,
) -> (Vec<u8>, Result<(), BeamError>) {
    run_io_args(reg, fmt, |ctx, reg| vec![build(ctx, reg)])
}

/// `io:format(Fmt, [Arg])` text, asserting the call succeeded.
fn io_text(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Eterm,
) -> String {
    let (out, result) = run_io(reg, fmt, build);
    assert!(result.is_ok(), "{fmt:?}: {result:?}");
    String::from_utf8(out).expect("the printer emits UTF-8")
}

/// `io_lib:format(Fmt, [Arg])` then `lists:flatten/1` — the directive's text, but
/// through the `io_lib` entry point (which returns a deep list).
fn io_lib_text(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Eterm,
) -> Vec<u8> {
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let format = byte_list(&mut ctx, fmt);
    let arg = build(&mut ctx, reg);
    let args = ctx.list(&[arg]).expect("a short arg list fits");
    let deep = reg
        .call("io_lib", "format", 2, &mut ctx, &[format, args])
        .expect("io_lib:format");
    let flat = reg
        .call("lists", "flatten", 1, &mut ctx, &[deep])
        .expect("lists:flatten/1");
    proper_bytes(flat)
}

/// The byte values of a proper character list (`signed_value` of each element).
fn proper_bytes(term: Eterm) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = beam_codec::term::cons_head(cur).expect("a live cons cell");
        let value = signed_value(head);
        assert!(
            (0..=255).contains(&value),
            "a character-list element out of byte range: {value}"
        );
        out.push(value as u8);
        cur = beam_codec::term::cons_tail(cur).expect("a live cons cell");
    }
    assert!(is_nil(cur), "not a proper list");
    out
}

/// Decodes a wire image through the registered `erlang:binary_to_term/1` — the
/// realistic route an external pid/port/ref enters the runtime by.
fn wire_term(ctx: &mut BifContext<'_>, reg: &BifRegistry, image: &[u8]) -> Eterm {
    let bin = binary(ctx, reg, image);
    reg.call("erlang", "binary_to_term", 1, ctx, &[bin])
        .expect("the wire image decodes")
}

/// Decodes `image` on a throwaway context and asserts `erts_internal:term_type/1`
/// names it `expected_type` (`external_port` / `external_pid` /
/// `external_reference`) — so the typed-gap assertions below cannot pass vacuously
/// (they must be about a term the printer really cannot render).
fn assert_decodes_as(image: &[u8], expected_type: &str) {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let term = wire_term(&mut ctx, &reg, image);
    let ty = reg
        .call("erts_internal", "term_type", 1, &mut ctx, &[term])
        .expect("term_type/1");
    let got = ctx
        .atoms()
        .name(atom_val(ty))
        .expect("a seeded type name")
        .to_string();
    assert_eq!(got, expected_type, "the wire image decoded as the wrong shape");
}

/// Runs `io:format(Fmt, Args)` under the system `erl`; `None` (with a loud
/// message) when the oracle is unavailable, so the cross-check skips rather than
/// fails.
fn oracle_format(fmt: &str, args: &[&str]) -> Option<Vec<u8>> {
    let statement = format!("io:format(\"{fmt}\", [{}]), halt().", args.join(","));
    match Command::new("erl")
        .args(["-noshell", "-eval", &statement])
        .output()
    {
        Ok(out) if out.status.success() => Some(out.stdout),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

/// The oracle's rendering of its own first port (`#Port<0.N>`) and the port number
/// `N` it printed; `None` when no oracle is available. The number is *parsed* rather
/// than assumed, so a live cross-check compares like with like.
fn oracle_local_port() -> Option<(u64, String)> {
    let out = oracle_format("~p", &["hd(erlang:ports())"])?;
    let text = String::from_utf8(out).ok()?;
    let number = text.strip_prefix("#Port<0.")?.strip_suffix('>')?.to_string();
    Some((number.parse().ok()?, text))
}

// ---------------------------------------------------------------------------
// A local port
// ---------------------------------------------------------------------------

/// A local port renders `#Port<0.Num>` — the local node's channel number and the
/// port-table number — identically on every entry point, and identically to the
/// oracle's rendering of its own port of the same number.
#[test]
fn a_local_port_renders_like_the_oracle() {
    let reg = default_registry();

    // Goldens: `erl -noshell -eval 'io:format("~p",[hd(erlang:ports())])'` ⇒
    // `#Port<0.0>` for the oracle's own first port; the number is the port-table
    // number, which is 7 here.
    assert_eq!(
        io_text(&reg, b"~p", |_, _| PortId::new(7).to_eterm()),
        "#Port<0.7>"
    );
    assert_eq!(
        io_text(&reg, b"~w", |_, _| PortId::new(7).to_eterm()),
        "#Port<0.7>"
    );
    // `io_lib:format/2` (the deep-list entry point) flattens to the same text.
    assert_eq!(
        io_lib_text(&reg, b"~p", |_, _| PortId::new(7).to_eterm()),
        b"#Port<0.7>".to_vec()
    );

    // Live cross-check against the oracle's own port number.
    let Some((number, oracle_text)) = oracle_local_port() else {
        return;
    };
    for fmt in [b"~p".as_slice(), b"~w".as_slice()] {
        let (out, result) = run_io(&reg, fmt, |_, _| PortId::new(number).to_eterm());
        assert!(result.is_ok(), "{fmt:?}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("the printer emits UTF-8"),
            oracle_text,
            "{fmt:?} of a local port"
        );
    }
}

/// A port nested in a list, a tuple and a map renders in place, exactly as the
/// oracle does (`nested_rec` below is the oracle's own input).
#[test]
fn a_port_nested_in_a_container_renders_like_the_oracle() {
    let reg = default_registry();
    let port = || PortId::new(7).to_eterm();

    assert_eq!(
        io_text(&reg, b"~p", |ctx, _| ctx
            .list(&[port()])
            .expect("a 1-element list fits")),
        "[#Port<0.7>]"
    );
    assert_eq!(
        io_text(&reg, b"~w", |ctx, _| {
            let one = ctx.int(1).expect("fits");
            ctx.tuple(&[port(), one]).expect("a 2-tuple fits")
        }),
        "{#Port<0.7>,1}"
    );
    assert_eq!(
        io_text(&reg, b"~p", |ctx, _| {
            let one = ctx.int(1).expect("fits");
            flat_map_of(ctx, &[(port(), one)])
        }),
        "#{#Port<0.7> => 1}"
    );

    // Live cross-check: the oracle's `~p` of `[Port,{Port},#{k => Port}]`.
    let Some((number, _)) = oracle_local_port() else {
        return;
    };
    let Some(expected) = oracle_format(
        "~p",
        &["[hd(erlang:ports()),{hd(erlang:ports())},#{k => hd(erlang:ports())}]"],
    ) else {
        return;
    };
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let p = PortId::new(number).to_eterm();
        let k = ctx.atom("k").expect("seeded");
        let map = flat_map_of(ctx, &[(k, p)]);
        let tuple = ctx.tuple(&[p]).expect("a 1-tuple fits");
        ctx.list(&[p, tuple, map]).expect("a 3-element list fits")
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, expected, "a port nested in a list, tuple and map");
}

// ---------------------------------------------------------------------------
// Depth-limited directives
// ---------------------------------------------------------------------------

/// `~W`/`~P` treat a port as an *atomic* leaf: `io_lib:write1/4` has no depth
/// clause for it (`write_record` and the containers are the only ones), so only
/// depth `0` elides it — and a port inside a container obeys the container's depth.
#[test]
fn depth_limited_port_directives_render_like_the_oracle() {
    let reg = default_registry();
    let with_depth = |fmt_depth: i64| {
        move |ctx: &mut BifContext<'_>, _: &BifRegistry| {
            vec![
                PortId::new(7).to_eterm(),
                ctx.int(fmt_depth).expect("fits"),
            ]
        }
    };

    // `~P`: depth 0 ⇒ `...`, depth ≥ 1 ⇒ the whole leaf.
    // Goldens: `~P` of `hd(erlang:ports())` at depth 0 ⇒ `...`, at 1 and 2 ⇒
    // `#Port<0.0>`.
    for (depth, want) in [(0i64, "..."), (1, "#Port<0.7>"), (2, "#Port<0.7>")] {
        let (out, result) = run_io_args(&reg, b"~P", with_depth(depth));
        assert!(result.is_ok(), "~P port {depth}: {result:?}");
        assert_eq!(String::from_utf8(out).expect("ASCII"), want, "~P port {depth}");
    }
    // `~W`: the flat depth renderer — the same leaf, the same depth rule.
    for (depth, want) in [(0i64, "..."), (1, "#Port<0.7>"), (2, "#Port<0.7>")] {
        let (out, result) = run_io_args(&reg, b"~W", with_depth(depth));
        assert!(result.is_ok(), "~W port {depth}: {result:?}");
        assert_eq!(String::from_utf8(out).expect("ASCII"), want, "~W port {depth}");
    }

    // `~W [Port] D`: the container consumes the depth, the leaf does not.
    // Goldens: `~W` of `[hd(erlang:ports())]` ⇒ `...` / `[...]` / `[#Port<0.0>]`.
    let list_with_depth = |fmt_depth: i64| {
        move |ctx: &mut BifContext<'_>, _: &BifRegistry| {
            let list = ctx
                .list(&[PortId::new(7).to_eterm()])
                .expect("a 1-element list fits");
            vec![list, ctx.int(fmt_depth).expect("fits")]
        }
    };
    for (depth, want) in [(0i64, "..."), (1, "[...]"), (2, "[#Port<0.7>]")] {
        let (out, result) = run_io_args(&reg, b"~W", list_with_depth(depth));
        assert!(result.is_ok(), "~W [port] {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~W [port] {depth}"
        );
    }

    // Live cross-check of the two depth-1 forms.
    let Some((number, _)) = oracle_local_port() else {
        return;
    };
    let n = number.to_string();
    let Some(exp_p1) = oracle_format("~P", &["hd(erlang:ports())", "1"]) else {
        return;
    };
    let (out, result) = run_io_args(&reg, b"~P", move |ctx, _| {
        vec![PortId::new(number).to_eterm(), ctx.int(1).expect("fits")]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, exp_p1, "~P port 1");
    let Some(exp_w1) = oracle_format("~W", &["[hd(erlang:ports())]", "1"]) else {
        return;
    };
    let (out, result) = run_io_args(&reg, b"~W", move |ctx, _| {
        let list = ctx
            .list(&[PortId::new(number).to_eterm()])
            .expect("a 1-element list fits");
        vec![list, ctx.int(1).expect("fits")]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out, exp_w1, "~W [port] 1");
    assert_eq!(n, number.to_string());
}

// ---------------------------------------------------------------------------
// External identifiers — the typed gap
// ---------------------------------------------------------------------------

/// A `V4_PORT_EXT` (`120`) naming the foreign node `a@b`, port id 7, creation 0 —
/// `[131][120][ATOM_UTF8_EXT "a@b"][id:8][creation:4]`.
const EXTERNAL_PORT_IMAGE: &[u8] = &[
    131, 120, 119, 3, b'a', b'@', b'b', 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0,
];

/// A `NEW_PID_EXT` (`88`) naming the foreign node `a@b`, id 1, serial 7, creation 0.
const EXTERNAL_PID_IMAGE: &[u8] = &[
    131, 88, 119, 3, b'a', b'@', b'b', 0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0,
];

/// A `NEWER_REFERENCE_EXT` (`90`) naming `a@b` with one 32-bit id (`5`), creation
/// 0 — `[131][90][words:2][ATOM_UTF8_EXT "a@b"][creation:4][id0:4]`.
const EXTERNAL_REF_IMAGE: &[u8] = &[
    131, 90, 0, 1, 119, 3, b'a', b'@', b'b', 0, 0, 0, 0, 0, 0, 0, 5,
];

/// The error class the printer reports for a term it cannot render
/// byte-faithfully: the typed `unsupported_bif`, never a guessed shape.
fn is_unsupported(result: &Result<(), BeamError>) -> bool {
    matches!(result, Err(BeamError::Raise(r)) if r.reason.as_atom() == Some("unsupported_bif"))
}

/// A decoded external port stays the typed gap: its channel number is the
/// *node-name atom's index in the running VM* (`atom_val(x->sysname)`,
/// `erl_node_container_utils.h:109-116`), which the frozen external-port term does
/// not carry. Oracle evidence: the same image printed `#Port<10643.7>` and
/// `#Port<10639.7>` in two runs. See the module docs.
#[test]
fn an_external_port_is_a_typed_gap() {
    assert_decodes_as(EXTERNAL_PORT_IMAGE, "external_port");

    let reg = default_registry();
    for fmt in [b"~p".as_slice(), b"~w".as_slice()] {
        let (out, result) = run_io(&reg, fmt, |ctx, reg| {
            wire_term(ctx, reg, EXTERNAL_PORT_IMAGE)
        });
        assert!(out.is_empty(), "{fmt:?} must print nothing: {out:?}");
        assert!(
            is_unsupported(&result),
            "{fmt:?} of an external port must be the typed gap: {result:?}"
        );
    }
}

/// A decoded external pid is the same typed gap — oracle `<10637.1.7>`, with the
/// same runtime-state channel (`atom_val` of the node atom).
#[test]
fn an_external_pid_is_a_typed_gap() {
    assert_decodes_as(EXTERNAL_PID_IMAGE, "external_pid");

    let reg = default_registry();
    for fmt in [b"~p".as_slice(), b"~w".as_slice()] {
        let (out, result) = run_io(&reg, fmt, |ctx, reg| {
            wire_term(ctx, reg, EXTERNAL_PID_IMAGE)
        });
        assert!(out.is_empty(), "{fmt:?} must print nothing: {out:?}");
        assert!(
            is_unsupported(&result),
            "{fmt:?} of an external pid must be the typed gap: {result:?}"
        );
    }
}

/// A decoded external reference is the same typed gap — oracle `#Ref<10643.5>`
/// (the channel, then every id word most significant first).
#[test]
fn an_external_ref_is_a_typed_gap() {
    assert_decodes_as(EXTERNAL_REF_IMAGE, "external_reference");

    let reg = default_registry();
    for fmt in [b"~p".as_slice(), b"~w".as_slice()] {
        let (out, result) = run_io(&reg, fmt, |ctx, reg| {
            wire_term(ctx, reg, EXTERNAL_REF_IMAGE)
        });
        assert!(out.is_empty(), "{fmt:?} must print nothing: {out:?}");
        assert!(
            is_unsupported(&result),
            "{fmt:?} of an external ref must be the typed gap: {result:?}"
        );
    }
}

/// An external identifier nested in a container is the same typed gap — the gap is
/// a property of the leaf, not of the entry point.
#[test]
fn an_external_port_nested_in_a_list_is_a_typed_gap() {
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p", |ctx, reg| {
        let port = wire_term(ctx, reg, EXTERNAL_PORT_IMAGE);
        ctx.list(&[port]).expect("a 1-element list fits")
    });
    assert!(out.is_empty(), "must print nothing: {out:?}");
    assert!(is_unsupported(&result), "{result:?}");
}

/// The **internal** port stays renderable — the control that keeps the typed-gap
/// tests above honest (the printer is not simply refusing every port).
#[test]
fn an_internal_port_is_still_rendered() {
    let reg = default_registry();
    assert_eq!(
        io_text(&reg, b"~p", |_, _| PortId::new(0).to_eterm()),
        "#Port<0.0>"
    );
}
