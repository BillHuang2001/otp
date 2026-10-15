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

//! Integration tests for the printer's **fun** leaf (`~w`/`~p`/`~W`/`~P`).
//!
//! The oracle's leaf for a fun is `erlang:fun_to_list/1`
//! (`lib/stdlib/src/io_lib.erl:914` for `~w`/`~p`; `io_lib_pretty:print_length/7`
//! at `lib/stdlib/src/io_lib_pretty.erl:932-934` splices the same `io_lib:write/1`
//! text), so a term containing a fun used to make the whole `io:format/2` raise
//! `badarg`. The defect was a missing leaf branch in
//! `beam_bifs::print::render_chunk`: every other leaf type had one, a
//! [`is_function`](beam_bifs)-shaped term fell through to the trailing
//! `Err(badarg())`, and because `render_chunk` recurses the raise happened for a
//! fun at **any** nesting depth.
//!
//! Ground truth (read-only): the `FUN_DEF` arm of the `%T` printer
//! (`erts/emulator/beam/erl_printf_term.c:674-724`) behind `fun_to_list` — a
//! **non-local** fun (our `FunKind::External`, an `EXPORT_EXT`) renders
//! `fun Module:Function/Arity` with C `print_atom_name` quoting and the **true**
//! arity, while a **local** fun (our `FunKind::Imported` from `NEW_FUN_EXT` *and*
//! our `FunKind::Local` from `make_fun3`) renders `#Fun<Module.OldIndex.OldUniq>`.
//! The tree inlines the C's out-of-line `ErlFunEntry` into the term descriptor, so
//! an `Imported` fun carries all three fields but a `Local` (`make_fun3`) fun —
//! descriptor `[uniq:u64][index:u32]` — carries none: that one stays the typed
//! capability gap, exactly like `erlang:fun_to_list/1` (`UnsupportedBif`), because
//! a fabricated `#Fun<…>` would be a *silently wrong* result.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] (plus
//! [`BytesSink`] for the `io:format/2` output) and the declared dependency crates
//! (`beam_alloc` for the heap, `beam_codec::term` for the fun constructors and the
//! list/map readers).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_codec::term::{
    cons_head, cons_tail, make_flatmap, make_fun_external, make_fun_imported, FunPid,
};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, is_nil, is_small, signed_value, AtomTable, Eterm};

/// The atoms the fixtures seed: the two external funs' modules/names, the map key,
/// the quoted-atom module, and `ok` (the `io:format/2` return value's atom).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["lists", "reverse", "erlang", "abs", "a", "foo", "nonode@nohost", "ok"]);
    atoms
}

/// A seeded atom on `ctx`'s heap.
fn atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("the atom is seeded")
}

/// A small integer on `ctx`'s heap.
fn int(ctx: &mut BifContext<'_>, value: i64) -> Eterm {
    ctx.int(value).expect("a small int fits")
}

/// A proper list of the (already built) `items` on `ctx`'s heap.
fn list(ctx: &mut BifContext<'_>, items: &[Eterm]) -> Eterm {
    ctx.list(items).expect("a short list fits")
}

/// A byte-aligned format list (`"~p"` / `"~w"` / `"~p~n"`) on `ctx`'s heap.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let elems: Vec<Eterm> = bytes.iter().map(|&b| int(ctx, i64::from(b))).collect();
    list(ctx, &elems)
}

/// An `EXPORT_EXT` fun (`fun Module:Function/Arity`) of the `(module, function,
/// arity)` `triple`, on `ctx`'s heap.
fn external_fun(ctx: &mut BifContext<'_>, triple: (&str, &str, usize)) -> Eterm {
    let (module, function, arity) = triple;
    let module = atom_val(atom(ctx, module));
    let function = atom_val(atom(ctx, function));
    make_fun_external(ctx.heap(), arity, module, function).expect("the external fun fits")
}

/// A `NEW_FUN_EXT` ([`FunKind::Imported`](beam_codec::term::FunKind)) fun on
/// `ctx`'s heap.
fn imported_fun(ctx: &mut BifContext<'_>, old_index: u32, old_uniq: u32) -> Eterm {
    let module = atom_val(atom(ctx, "lists"));
    let creator = FunPid {
        node: atom_val(atom(ctx, "nonode@nohost")),
        number: 1,
        serial: 0,
        creation: 0,
    };
    make_fun_imported(ctx.heap(), 1, module, &[0u8; 16], 7, old_index, old_uniq, creator, &[])
        .expect("the imported fun fits")
}

/// A `FunKind::Local` (`make_fun3`) block: the header word plus the
/// `[uniq, index]` descriptor, with no module/name anywhere in the term. Held in a
/// `Box` the caller keeps alive for the duration of the call.
fn local_fun_words() -> Box<[Eterm]> {
    vec![
        Eterm::from_raw(beam_term::FUN_SUBTAG | (2usize << 8)),
        Eterm::from_raw(0), // uniq
        Eterm::from_raw(0), // index
    ]
    .into_boxed_slice()
}

// ---------------------------------------------------------------------------
// (a) The committed goldens (no oracle)
// ---------------------------------------------------------------------------

/// One golden case: the directive, the argument term builder and the exact text
/// the system oracle produced for it (OTP 29.1.1 / erts-17.1, captured with
/// `erl -noshell -eval 'io:format("<directive>~n",[<expr>]),halt(0).'`).
struct Golden {
    name: &'static str,
    directive: &'static str,
    build: fn(&mut BifContext<'_>) -> Eterm,
    expect: &'static str,
    /// Whether the built term **is** the fun itself (so `erlang:fun_to_list/1`,
    /// which takes a fun — not the container — can be cross-checked on it).
    fun_leaf: bool,
}

/// The `(module, function, arity)` of the two external funs the cases use.
const REVERSE: (&str, &str, usize) = ("lists", "reverse", 1);
const ABS: (&str, &str, usize) = ("erlang", "abs", 1);

fn goldens() -> Vec<Golden> {
    vec![
        Golden {
            name: "p/external-fun",
            directive: "~p",
            build: |c| external_fun(c, REVERSE),
            expect: "fun lists:reverse/1",
            fun_leaf: true,
        },
        Golden {
            name: "w/external-fun",
            directive: "~w",
            build: |c| external_fun(c, REVERSE),
            expect: "fun lists:reverse/1",
            fun_leaf: true,
        },
        Golden {
            name: "p/fun-in-list",
            directive: "~p",
            build: |c| {
                let fun = external_fun(c, REVERSE);
                let one = int(c, 1);
                list(c, &[one, fun])
            },
            expect: "[1,fun lists:reverse/1]",
            fun_leaf: false,
        },
        Golden {
            name: "w/fun-in-list",
            directive: "~w",
            build: |c| {
                let fun = external_fun(c, REVERSE);
                let one = int(c, 1);
                list(c, &[one, fun])
            },
            expect: "[1,fun lists:reverse/1]",
            fun_leaf: false,
        },
        Golden {
            name: "p/fun-in-tuple",
            directive: "~p",
            build: |c| {
                let fun = external_fun(c, REVERSE);
                let two = int(c, 2);
                c.tuple(&[two, fun]).expect("a 2-tuple fits")
            },
            expect: "{2,fun lists:reverse/1}",
            fun_leaf: false,
        },
        Golden {
            name: "w/fun-in-tuple",
            directive: "~w",
            build: |c| {
                let fun = external_fun(c, REVERSE);
                let two = int(c, 2);
                c.tuple(&[two, fun]).expect("a 2-tuple fits")
            },
            expect: "{2,fun lists:reverse/1}",
            fun_leaf: false,
        },
        Golden {
            name: "p/fun-in-map",
            directive: "~p",
            build: |c| {
                let key = atom(c, "a");
                let value = external_fun(c, REVERSE);
                make_flatmap(c.heap(), &[(key, value)]).expect("the map fits")
            },
            expect: "#{a => fun lists:reverse/1}",
            fun_leaf: false,
        },
        Golden {
            name: "w/fun-in-map",
            directive: "~w",
            build: |c| {
                let key = atom(c, "a");
                let value = external_fun(c, REVERSE);
                make_flatmap(c.heap(), &[(key, value)]).expect("the map fits")
            },
            expect: "#{a => fun lists:reverse/1}",
            fun_leaf: false,
        },
        Golden {
            name: "p/quoted-atom-mfa",
            directive: "~p",
            build: |c| {
                let module = atom_val(atom(c, "nonode@nohost"));
                let function = atom_val(atom(c, "foo"));
                make_fun_external(c.heap(), 1, module, function).expect("the external fun fits")
            },
            expect: "fun 'nonode@nohost':foo/1",
            fun_leaf: true,
        },
        Golden {
            name: "p/imported-new-fun-ext",
            directive: "~p",
            build: |c| imported_fun(c, 3, 1234),
            expect: "#Fun<lists.3.1234>",
            fun_leaf: true,
        },
    ]
}

/// Flattens an `io_lib:format` result (a nested char list of small integers) to
/// its text — the same reader `tests/io_lib_format_ts.rs` uses.
fn flatten_chars(term: Eterm) -> String {
    let mut out = String::new();
    let mut cursor = term;
    loop {
        if is_nil(cursor) {
            return out;
        }
        let head = cons_head(cursor).expect("a proper list of chars");
        if is_small(head) {
            let byte = u8::try_from(signed_value(head)).expect("a byte-sized char");
            out.push(byte as char);
        } else {
            out.push_str(&flatten_chars(head));
        }
        cursor = cons_tail(cursor).expect("a proper list of chars");
    }
}

/// Runs the registered `io_lib:format/2` for `directive` over `args` and returns
/// the rendered text.
fn io_lib_text(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    directive: &str,
    args: &[Eterm],
) -> Result<String> {
    let format_term = byte_list(ctx, directive.as_bytes());
    let args_term = list(ctx, args);
    let rendered = reg.call("io_lib", "format", 2, ctx, &[format_term, args_term])?;
    Ok(flatten_chars(rendered))
}

/// Every golden above renders the committed oracle text through the registered
/// `io_lib:format/2`, under **both** `~p` and `~w`, for the fun alone and nested at
/// any depth.
///
/// This runs with no oracle — the goldens are the literal oracle output, so the
/// behaviour is pinned even in a sandbox without `erl`.
#[test]
fn funs_render_as_fun_to_list_text() {
    let reg = default_registry();
    let cases = goldens();
    assert!(!cases.is_empty(), "the pinned fun corpus must not be empty");

    for case in &cases {
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let arg = (case.build)(&mut ctx);
        let got = io_lib_text(&mut ctx, &reg, case.directive, &[arg]);
        assert_eq!(
            got.expect("a rendered fun is not a typed gap"),
            case.expect,
            "case `{}`: io_lib:format(\"{}\", …) does not match the oracle",
            case.name,
            case.directive,
        );

        // For the cases whose argument **is** the fun, the printer's leaf and
        // `erlang:fun_to_list/1` must be the *same* text — the oracle's own
        // definition of the leaf. (`fun_to_list/1` takes a fun, not a container, so
        // the nested cases have nothing to cross-check here.)
        if case.fun_leaf {
            let via_bif = match reg.call("erlang", "fun_to_list", 1, &mut ctx, &[arg]) {
                Ok(term) => term,
                Err(e) => panic!("case `{}`: erlang:fun_to_list/1 => {e:?}", case.name),
            };
            assert_eq!(
                flatten_chars(via_bif),
                case.expect,
                "case `{}`: the printer disagrees with erlang:fun_to_list/1",
                case.name,
            );
        }
    }
}

/// `~W`/`~P` (a depth-limited render) splice the same fun text: a fun is a leaf, so
/// a depth limit never truncates it.
#[test]
fn depth_limited_directives_render_the_same_fun_text() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `~W`/`~P` take the depth from the *next* argument, so render `[1, Fun]` at
    // depth 3: the container is walked, the fun leaf is not truncated.
    let fun = external_fun(&mut ctx, REVERSE);
    let one = int(&mut ctx, 1);
    let arg = list(&mut ctx, &[one, fun]);
    let format_term = byte_list(&mut ctx, b"~P");
    let depth = int(&mut ctx, 3);
    let args_term = ctx.list(&[arg, depth]).expect("fits");
    let rendered = reg
        .call("io_lib", "format", 2, &mut ctx, &[format_term, args_term])
        .expect("io_lib:format(\"~P\", [Term, 3])");
    assert_eq!(flatten_chars(rendered), "[1,fun lists:reverse/1]");

    let format_term = byte_list(&mut ctx, b"~W");
    let depth = int(&mut ctx, 3);
    let args_term = ctx.list(&[arg, depth]).expect("fits");
    let rendered = reg
        .call("io_lib", "format", 2, &mut ctx, &[format_term, args_term])
        .expect("io_lib:format(\"~W\", [Term, 3])");
    assert_eq!(flatten_chars(rendered), "[1,fun lists:reverse/1]");
}

// ---------------------------------------------------------------------------
// (d) The remaining gap: a `make_fun3` (`FunKind::Local`) fun
// ---------------------------------------------------------------------------

/// A `FunKind::Local` (`make_fun3`) fun is the typed capability gap — **not** a
/// fabricated `#Fun<…>`.
///
/// Its inlined descriptor is only `[header, uniq, index]`, so the module/name a
/// `#Fun<M.OldIndex.OldUniq>` needs is not in the term; `erlang:fun_to_list/1` is
/// the same gap (pinned by `print.rs`'s in-module test), so the printer must stay
/// consistent with it rather than print a wrong `#Fun<…>`.
#[test]
fn a_local_fun_is_the_typed_capability_gap() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let words = local_fun_words();
    let fun = beam_term::make_boxed(words.as_ptr());

    // The failure *class* is the capability gap: an `unsupported_bif` raise, never
    // `badarg` (the fun argument is valid) and never a printed `#Fun<…>`.
    for directive in ["~p", "~w"] {
        let got = io_lib_text(&mut ctx, &reg, directive, &[fun]);
        assert!(
            matches!(
                got,
                Err(BeamError::Raise(ref r)) if r.reason.as_atom() == Some("unsupported_bif")
            ),
            "`{directive}` of a Local fun must be the typed unsupported_bif gap, got {got:?}",
        );
    }

    // Nested inside a container is the same typed gap (the recursion propagates
    // the leaf error rather than swallowing it).
    let one = int(&mut ctx, 1);
    let nested = list(&mut ctx, &[one, fun]);
    assert!(matches!(
        io_lib_text(&mut ctx, &reg, "~p", &[nested]),
        Err(BeamError::Raise(ref r)) if r.reason.as_atom() == Some("unsupported_bif")
    ));

    // `erlang:fun_to_list/1` is the same gap — the printer and the BIF agree.
    assert!(matches!(
        reg.call("erlang", "fun_to_list", 1, &mut ctx, &[fun]),
        Err(BeamError::Raise(ref r)) if r.reason.as_atom() == Some("unsupported_bif")
    ));

    drop(words);
}

// ---------------------------------------------------------------------------
// (b) The live-oracle cross-check (byte-exact, fails loudly)
// ---------------------------------------------------------------------------

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs print_fun] $ERTS_ORACLE='{}' is not an existing executable.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs one `erl -noshell -eval <eval>` (the eval must `halt()`) and returns stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// One cross-check case: the directive, the Rust-side term builder, and the
/// **Erlang source of the very same term**, derived from the same
/// `(module, function, arity)` triples the builder uses (never by printing the fun
/// on the Rust side — that would make the check circular).
struct CrossCheck {
    name: &'static str,
    directive: &'static str,
    build: fn(&mut BifContext<'_>) -> Eterm,
    source: fn() -> String,
}

/// `name` as Erlang source: bare when it is a run of lowercase letters/digits/`_`/`@`
/// starting with a lowercase letter, `'…'`-quoted otherwise.
fn erl_atom(name: &str) -> String {
    let mut chars = name.chars();
    let bare = match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')
        }
        _ => false,
    };
    if bare {
        name.to_string()
    } else {
        format!("'{name}'")
    }
}

/// The Erlang source `fun Module:Function/Arity` of an external-fun triple.
fn erl_fun_source(triple: (&str, &str, usize)) -> String {
    let (module, function, arity) = triple;
    format!("fun {}:{}/{arity}", erl_atom(module), erl_atom(function))
}

fn cross_checks() -> Vec<CrossCheck> {
    vec![
        CrossCheck {
            name: "p/external-fun",
            directive: "~p",
            build: |c| external_fun(c, REVERSE),
            source: || erl_fun_source(REVERSE),
        },
        CrossCheck {
            name: "p/external-fun-erlang-abs",
            directive: "~p",
            build: |c| external_fun(c, ABS),
            source: || erl_fun_source(ABS),
        },
        CrossCheck {
            name: "p/fun-nested-in-list-and-tuple",
            directive: "~p",
            build: |c| {
                let first = external_fun(c, REVERSE);
                let second = external_fun(c, ABS);
                let two = int(c, 2);
                let one = int(c, 1);
                let tuple = c.tuple(&[two, second]).expect("a 2-tuple fits");
                list(c, &[one, first, tuple])
            },
            source: || {
                format!(
                    "[1, {}, {{2, {}}}]",
                    erl_fun_source(REVERSE),
                    erl_fun_source(ABS)
                )
            },
        },
        CrossCheck {
            name: "p/fun-as-a-map-value",
            directive: "~p",
            build: |c| {
                let key = atom(c, "a");
                let value = external_fun(c, REVERSE);
                make_flatmap(c.heap(), &[(key, value)]).expect("the map fits")
            },
            source: || format!("#{{a => {}}}", erl_fun_source(REVERSE)),
        },
        CrossCheck {
            name: "w/fun-nested-in-list-and-tuple",
            directive: "~w",
            build: |c| {
                let first = external_fun(c, REVERSE);
                let second = external_fun(c, ABS);
                let two = int(c, 2);
                let one = int(c, 1);
                let tuple = c.tuple(&[two, second]).expect("a 2-tuple fits");
                list(c, &[one, first, tuple])
            },
            source: || {
                format!(
                    "[1, {}, {{2, {}}}]",
                    erl_fun_source(REVERSE),
                    erl_fun_source(ABS)
                )
            },
        },
        CrossCheck {
            name: "w/fun-as-a-map-value",
            directive: "~w",
            build: |c| {
                let key = atom(c, "a");
                let value = external_fun(c, REVERSE);
                make_flatmap(c.heap(), &[(key, value)]).expect("the map fits")
            },
            source: || format!("#{{a => {}}}", erl_fun_source(REVERSE)),
        },
    ]
}

/// Renders `directive~n` over `arg` through the registered **`io:format/2`** BIF
/// into a [`BytesSink`] and returns the captured bytes — the byte stream the
/// oracle's stdout is compared against.
fn io_format_bytes(
    heap: &mut Heap,
    atoms: &AtomTable,
    sink: &mut BytesSink,
    reg: &BifRegistry,
    directive: &str,
    arg: Eterm,
) -> Result<()> {
    let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0)).with_output(sink);
    let format_term = byte_list(&mut ctx, format!("{directive}~n").as_bytes());
    let args_term = list(&mut ctx, &[arg]);
    reg.call("io", "format", 2, &mut ctx, &[format_term, args_term])?;
    Ok(())
}

/// Re-derives every case against the **live** oracle and requires the captured
/// `io:format/2` byte stream to agree byte-for-byte with the oracle's stdout.
///
/// A missing oracle prints a LOUD warning and skips — `io:format/2` of a fun is an
/// end-to-end printer behaviour whose only independent reference is a real Erlang,
/// so there is nothing to pin without one (the goldens in
/// [`funs_render_as_fun_to_list_text`] still run in that case).
#[test]
fn funs_cross_check_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!(
            "[beam_bifs print_fun] WARNING: no system `erl` found ($ERTS_ORACLE naming an \
             executable, or one on $PATH) — SKIPPING the live-oracle cross-check of the \
             printer's fun leaf. Set $ERTS_ORACLE to run it."
        );
        return;
    };

    let reg = default_registry();
    let cases = cross_checks();
    assert!(!cases.is_empty(), "the cross-check corpus must not be empty");

    for case in &cases {
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded_atoms();
        let mut sink = BytesSink::new();
        let source = (case.source)();
        // The fun term and the format argument live on the same heap, but the
        // `with_output` context is built by [`io_format_bytes`].
        let arg = {
            let mut build_ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
            (case.build)(&mut build_ctx)
        };
        io_format_bytes(&mut heap, &atoms, &mut sink, &reg, case.directive, arg)
            .unwrap_or_else(|e| panic!("case `{}`: io:format failed: {e:?}", case.name));
        let ours = sink.contents().to_vec();

        let eval = format!("io:format(\"{}~n\",[{source}]),halt(0).", case.directive);
        let want = run_erl(&oracle, &eval);

        assert_eq!(
            String::from_utf8_lossy(&ours),
            want,
            "case `{}`: `io:format(\"{}~n\",[{}])` disagrees with the live oracle\n\
             --- ours (bytes) ---\n{ours:?}\n--- oracle (bytes) ---\n{want:?}",
            case.name,
            case.directive,
            source,
        );
    }
}

// ---------------------------------------------------------------------------
// (c) The end-to-end `beam_rt` variant of the raw repro
// ---------------------------------------------------------------------------

/// The `beam_rt` binary of this checkout, if it has been built: `$CARGO_TARGET_DIR`
/// when set, else `<workspace>/target/{debug,release}`.
fn beam_rt_binary() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        roots.push(PathBuf::from(dir));
    }
    // `CARGO_MANIFEST_DIR` is `<workspace>/crates/beam_bifs`.
    if let Some(workspace) = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent) {
        roots.push(workspace.join("target"));
    }
    for root in roots {
        for profile in ["debug", "release"] {
            let candidate = root.join(profile).join("beam_rt");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// The original end-to-end repro: `beam_rt -noshell -eval
/// 'io:format("~p~n",[fun lists:reverse/1]),halt(0).'` must print the fun's text and
/// exit 0 (it used to print `beam_rt: badarg` and exit 1). `-noshell` is required so
/// the runtime prints no startup banner ahead of the result (the oracle is also run
/// with `-noshell`).
///
/// The `beam_rt` binary is only present when the workspace has been built beyond
/// `-p beam_bifs`, so a missing binary prints a LOUD warning and skips.
#[test]
fn beam_rt_prints_a_fun_in_byte_code_eval() {
    let Some(binary) = beam_rt_binary() else {
        eprintln!(
            "[beam_bifs print_fun] WARNING: no `beam_rt` binary found under \
             $CARGO_TARGET_DIR or <workspace>/target/{{debug,release}} — SKIPPING the \
             end-to-end repro (build it with `cargo build -p beam_rt`)."
        );
        return;
    };

    let eval = "io:format(\"~p~n\",[fun lists:reverse/1]),halt(0).";
    let output = Command::new(&binary)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn '{}': {e}", binary.display()));
    let stdout = String::from_utf8(output.stdout).expect("beam_rt printed non-UTF-8");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        stdout, "fun lists:reverse/1\n",
        "the raw `~p` repro must print the fun's text\n--- stderr ---\n{stderr}\n--- eval ---\n{eval}",
    );
    assert!(
        output.status.success(),
        "`beam_rt -noshell -eval '{eval}'` exited with {:?}\n--- stderr ---\n{stderr}",
        output.status.code(),
    );

    // When an oracle is available, the end-to-end text must equal its stdout too.
    if let Some(oracle) = resolve_oracle() {
        assert_eq!(stdout, run_erl(&oracle, eval), "beam_rt disagrees with the oracle");
    }
}
