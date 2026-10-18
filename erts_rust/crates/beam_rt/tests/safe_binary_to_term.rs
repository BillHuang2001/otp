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

//! The `[safe]` `binary_to_term/2` **active-export gate** end to end (contract A.2).
//!
//! C's `EXPORT_EXT` arm (`erts/emulator/beam/external.c:5006-5040`) refuses an `EXPORT_EXT`
//! (`113`) fun under the `ERTS_DIST_EXT_BTT_SAFE` flag unless
//! `erts_active_export_entry(mod, name, arity)` — "is `M:F/A` exported by code loaded *right
//! now*?" — holds. The codec owns the check site (`beam_codec::decode::fun`), the runtime owns
//! the registry: `RuntimeContext`'s `erlang:binary_to_term/2` dispatch arm installs
//! `ExportGateBridge` (`beam_rt::context`), so this suite drives the **whole** route
//! (`call_bif` → `beam_bifs`' `binconv::decode_term_bif` → the codec's gate) and compares its
//! verdicts with the live oracle's.
//!
//! # Why the images are built by hand
//!
//! The `-eval` grammar has no named-fun form (`fun M:F/A` — `src/eval/CONTEXT.md`), so the
//! input cannot be produced here as `term_to_binary(fun lists:map/2)`. The `EXPORT_EXT` wire
//! form is tiny and fully specified — `[131][113][module atom][function atom][arity term]` — so
//! [`export_image`] writes it directly. The oracle is then asked, per case, whether the **same**
//! bytes decode (flag-free) to *its own* `fun M:F/A` — compared through `erlang:fun_info/2`,
//! because merely constructing `fun M:F/A` on the oracle would **activate** that export and so
//! perturb the very gate under test (see [`oracle_case`]). That `same=true` is what proves the
//! hand-built image is the term the compiler would have produced, and it is asserted for every
//! oracle-compared case rather than assumed.
//!
//! # The verdict matrix (oracle-verified, OTP 29.1.1 / erts-17.1)
//!
//! | `M:F/A` | `[safe]` | why |
//! |---|---|---|
//! | `lists:map/2` | accept | a loaded stdlib export on the oracle; a registered `beam_bifs` fast path here |
//! | `erlang:system_info/1` | accept | an emulator builtin (`bif.tab`) |
//! | `lists:map/3` | `error:badarg` | the wrong arity of a real function |
//! | `lists:no_such_fun_xyz/1` | `error:badarg` | no such function |
//! | `erlang:no_such_bif_xyz/1` | `error:badarg` | no such BIF |
//! | `mnesia:start/0` | `error:badarg` | a module that is not loaded in a bare node |
//!
//! Every one of those images decodes **unchanged** without `[safe]` — and with `[used]` — on
//! both runtimes, which is the control showing the gate is never consulted off the flag (the C
//! asks only under `ERTS_DIST_EXT_BTT_SAFE`).
//!
//! A missing toolchain, or a probe that cannot answer, is a **loud skip**
//! (`support::require_oracle` / `support::skip_loudly`), never a silent pass.

mod support;

use std::path::Path;

use beam_codec::FunKind;
use beam_common::Result;
use beam_engine::{terms, ExecutionContext};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::Eterm;

use support::{oracle_eval, require_oracle, skip_loudly};

/// This crate's failed-head-clause fixture: the module `function_clause_fixture` exports `f/1`
/// and `module_info/0,1`. The tests load it themselves (it is not one of the 22 preloaded
/// modules), so it is the end-to-end evidence for the **loaded-module** half of the gate.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/function_clause_fixture.beam"
);

/// What the `[safe]` decode must answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// The fun materializes.
    Accept,
    /// The decode is `error:badarg`.
    BadArg,
}

impl Verdict {
    /// The rendering both sides compare with: the atom `accept`, or the caught
    /// `{error,badarg}` tuple the oracle's `try … catch C:R -> {C,R} end` prints.
    fn rendered(self) -> &'static str {
        match self {
            Verdict::Accept => "accept",
            Verdict::BadArg => "{error,badarg}",
        }
    }
}

/// One `EXPORT_EXT` probe: the name on the wire and the verdict both runtimes must give it.
struct Case {
    /// The wire module name.
    module: &'static str,
    /// The wire function name.
    function: &'static str,
    /// The wire arity.
    arity: u8,
    /// The verdict `[safe]` must produce.
    expected: Verdict,
}

impl Case {
    /// `module:function/arity`, for assertion messages.
    fn label(&self) -> String {
        format!("{}:{}/{}", self.module, self.function, self.arity)
    }

    /// The wire image of this case.
    fn image(&self) -> Vec<u8> {
        export_image(self.module, self.function, self.arity)
    }
}

/// The oracle-compared half of the matrix (a bare node answers all six the same way this
/// runtime does).
const ORACLE_CASES: &[Case] = &[
    Case {
        module: "lists",
        function: "map",
        arity: 2,
        expected: Verdict::Accept,
    },
    Case {
        module: "erlang",
        function: "system_info",
        arity: 1,
        expected: Verdict::Accept,
    },
    Case {
        module: "lists",
        function: "map",
        arity: 3,
        expected: Verdict::BadArg,
    },
    Case {
        module: "lists",
        function: "no_such_fun_xyz",
        arity: 1,
        expected: Verdict::BadArg,
    },
    Case {
        module: "erlang",
        function: "no_such_bif_xyz",
        arity: 1,
        expected: Verdict::BadArg,
    },
    Case {
        module: "mnesia",
        function: "start",
        arity: 0,
        expected: Verdict::BadArg,
    },
];

/// The loaded-module half, asserted on this runtime only — the oracle never loads the fixture,
/// so it would refuse both cases (correctly, for its own state). `function_clause_fixture`
/// exports `f/1` (and `module_info/0,1`) and nothing else, so `f/2` is the negative half.
const LOADED_CASES: &[Case] = &[
    Case {
        module: "function_clause_fixture",
        function: "f",
        arity: 1,
        expected: Verdict::Accept,
    },
    Case {
        module: "function_clause_fixture",
        function: "f",
        arity: 2,
        expected: Verdict::BadArg,
    },
    Case {
        module: "function_clause_fixture",
        function: "no_such_fun_xyz",
        arity: 1,
        expected: Verdict::BadArg,
    },
];

/// Appends `name` as an `ATOM_EXT` (`100`): a 2-byte big-endian length then the Latin-1 bytes
/// (every name here is ASCII, so its UTF-8 bytes are its Latin-1 bytes).
fn push_atom(image: &mut Vec<u8>, name: &str) {
    assert!(name.is_ascii(), "the wire atoms here are ASCII: {name:?}");
    let len = u16::try_from(name.len()).expect("a wire atom this suite builds is short");
    image.push(100);
    image.extend_from_slice(&len.to_be_bytes());
    image.extend_from_slice(name.as_bytes());
}

/// The `EXPORT_EXT` (`113`) image of `module:function/arity`:
/// `[131][113][module atom][function atom][SMALL_INTEGER_EXT arity]`.
///
/// The arity is a **full term** on the wire (`external.c:5025-5031`), and a small integer is
/// what the compiler writes for any arity this suite uses.
fn export_image(module: &str, function: &str, arity: u8) -> Vec<u8> {
    let mut image = vec![131u8, 113u8];
    push_atom(&mut image, module);
    push_atom(&mut image, function);
    image.push(97); // SMALL_INTEGER_EXT
    image.push(arity);
    image
}

/// The image as the comma-separated decimal list an Erlang binary literal wants.
fn image_literal(image: &[u8]) -> String {
    image
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// A fresh runtime with the fixture loaded and `names` interned into its shared atom table.
///
/// The names are interned **before** the decode on purpose: under `[safe]` an *unknown* wire
/// atom name is `badarg` from the codec (`erts_atom_get`, `external.c:4530-4539`), so the names
/// must already be present for a refusal to be attributable to the **gate** rather than to the
/// atom rule. The oracle probe interns them the same way (`list_to_atom/1`, never by creating
/// the fun — that would activate the export; see [`oracle_case`]).
fn runtime(names: &[&str]) -> (Runtime, Vec<Eterm>) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(FIXTURE).expect("the committed fixture loads");
    let atoms = names.iter().map(|name| rt.intern(name)).collect();
    (rt, atoms)
}

/// Runs `erlang:binary_to_term/1,2` through the runtime's real dispatch
/// (`RuntimeContext::call_bif`) — the route a compiled call and the `-eval` evaluator take.
fn decode(rt: &mut Runtime, image: &[u8], options: &[Eterm]) -> Result<Eterm> {
    let mut ctx = RuntimeContext::new(rt);
    let binary = terms::make_heap_binary(&mut ctx, image).expect("a heap-binary ETF image");
    if options.is_empty() {
        ctx.call_bif("erlang", "binary_to_term", 1, &[binary])
    } else {
        let opts = terms::make_list(&mut ctx, options).expect("the options list fits");
        ctx.call_bif("erlang", "binary_to_term", 2, &[binary, opts])
    }
}

/// Renders a decode outcome exactly as the oracle probe prints it: the atom `accept`, or
/// `{error,<reason atom>}` for a raise.
fn render(result: Result<Eterm>) -> String {
    match result {
        Ok(_) => "accept".to_string(),
        Err(error) => match error.as_atom() {
            Some(atom) => format!("{{error,{atom}}}"),
            None => format!("{{error,{error}}}"),
        },
    }
}

/// The oracle's four answers for one case.
struct OracleVerdicts {
    /// Whether the hand-built image decodes to the oracle's own `fun M:F/A`.
    same: bool,
    /// The `[safe]` verdict.
    safe: String,
    /// The flag-free verdict (must be `accept`).
    plain: String,
    /// The `[used]` verdict (must be `accept`: a `{Term, N}` tuple came back).
    used: String,
}

/// Asks the oracle about one case.
///
/// # The probe must not create `fun M:F/A` — it *activates* the export
///
/// The obvious way to intern the two names (and to obtain the term to compare the decoded
/// image with) is `S = fun M:F/A`. It is also **wrong**: constructing that fun on the oracle
/// registers an active export entry for `M:F/A` — measured, `fun lists:map/3` (a function
/// `lists` does not export) makes a following `binary_to_term(B, [safe])` of the very same
/// image answer `accept` instead of `{error,badarg}`. The probe would then be asking about the
/// gate *after* perturbing it.
///
/// So the two names are interned with `list_to_atom/1` (which only interns), and the "is this
/// the compiler's own wire form?" question is answered from the **flag-free** decode with
/// `erlang:fun_info/2` — no fun is ever constructed.
///
/// `None` means the probe could not answer (the caller then skips loudly).
fn oracle_case(oracle: &Path, case: &Case) -> Option<OracleVerdicts> {
    let (module, function, arity) = (case.module, case.function, case.arity);
    let bytes = image_literal(&case.image());
    // A raw string keeps the Erlang source readable: every `{`/`}` below is Erlang syntax.
    let eval = format!(
        r#"_ = list_to_atom("{module}"), _ = list_to_atom("{function}"),
B = <<{bytes}>>,
Same = case binary_to_term(B) of
         T when is_function(T) ->
           [element(2, erlang:fun_info(T, K)) || K <- [module, name, arity]]
             =:= [list_to_atom("{module}"), list_to_atom("{function}"), {arity}];
         _ -> false
       end,
Safe = try binary_to_term(B,[safe]) of _ -> accept catch C:R -> {{C,R}} end,
Plain = try binary_to_term(B) of _ -> accept catch C2:R2 -> {{C2,R2}} end,
Used = try binary_to_term(B,[used]) of {{_,N}} when is_integer(N) -> accept; _ -> other catch C3:R3 -> {{C3,R3}} end,
io:format("same=~p~nsafe=~p~nplain=~p~nused=~p~n", [Same, Safe, Plain, Used]),
halt()."#
    );
    let stdout = oracle_eval(oracle, None, &eval)?;
    let field = |tag: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(tag))
            .map(str::to_string)
    };
    Some(OracleVerdicts {
        same: field("same=").as_deref() == Some("true"),
        safe: field("safe=")?,
        plain: field("plain=")?,
        used: field("used=")?,
    })
}

#[test]
fn the_safe_export_gate_accepts_and_refuses_exactly_what_the_oracle_does() {
    let Some(oracle) = require_oracle("safe_binary_to_term") else {
        return;
    };
    for case in ORACLE_CASES {
        let label = case.label();
        let image = case.image();
        // The shared atom table must already hold both names (see `runtime`), plus the two
        // option atoms.
        let (mut rt, atoms) = runtime(&[case.module, case.function, "safe", "used"]);
        let safe = [atoms[2]];
        let used = [atoms[3]];

        let runtime_safe = render(decode(&mut rt, &image, &safe));
        let runtime_plain = render(decode(&mut rt, &image, &[]));
        let runtime_used = render(decode(&mut rt, &image, &used));

        let Some(oracle_verdicts) = oracle_case(&oracle, case) else {
            skip_loudly("safe_binary_to_term", "the oracle could not answer one case");
            return;
        };

        // The hand-built wire image is the oracle's own `fun M:F/A` term — otherwise the rest
        // of this comparison would be about a term nobody ever produces.
        assert!(
            oracle_verdicts.same,
            "the hand-built EXPORT_EXT image of {label} must decode to the oracle's own \
             `fun {label}`, got {:?}",
            oracle_verdicts.safe
        );
        // The pinned matrix, on both runtimes.
        assert_eq!(
            oracle_verdicts.safe,
            case.expected.rendered(),
            "the oracle's `[safe]` verdict for {label} changed"
        );
        assert_eq!(
            runtime_safe,
            case.expected.rendered(),
            "this runtime's `[safe]` verdict for {label}"
        );
        // …and the two agree, which is the actual compatibility claim.
        assert_eq!(
            runtime_safe, oracle_verdicts.safe,
            "the `[safe]` verdicts for {label} diverged"
        );
        // The gate is inert off the flag, on both sides: `binary_to_term/1` and
        // `binary_to_term(_, [used])` decode the very same image.
        assert_eq!(
            oracle_verdicts.plain, "accept",
            "the oracle's flag-free decode of {label}"
        );
        assert_eq!(
            runtime_plain, "accept",
            "this runtime's flag-free decode of {label}"
        );
        assert_eq!(
            oracle_verdicts.used, "accept",
            "the oracle's `[used]` decode of {label}"
        );
        assert_eq!(
            runtime_used, "accept",
            "this runtime's `[used]` decode of {label}"
        );
    }
}

#[test]
fn an_accepted_export_materializes_as_an_external_fun_of_that_exact_name() {
    let (mut rt, atoms) = runtime(&["lists", "map", "safe"]);
    let image = export_image("lists", "map", 2);
    let term = decode(&mut rt, &image, &[atoms[2]]).expect("an active export decodes");

    assert_eq!(
        beam_codec::term::fun_kind_term(term).expect("a fun"),
        FunKind::External,
        "an EXPORT_EXT decodes to an `External` fun"
    );
    assert_eq!(
        beam_codec::term::fun_module(term).expect("a fun"),
        rt.atoms().lookup("lists").expect("interned")
    );
    assert_eq!(
        beam_codec::term::fun_function(term).expect("a fun"),
        rt.atoms().lookup("map").expect("interned")
    );
    assert_eq!(beam_codec::term::fun_arity_term(term).expect("a fun"), 2);
}

#[test]
fn the_loaded_module_half_of_the_gate_is_live() {
    // The oracle never loads this fixture, so these are asserted on this runtime only: a module
    // the code server holds **now** must have its real exports accepted, and an arity or a
    // function it does not export must still be refused (the cases are not vacuous).
    for case in LOADED_CASES {
        let (mut rt, atoms) = runtime(&[case.module, case.function, "safe"]);
        let verdict = render(decode(&mut rt, &case.image(), &[atoms[2]]));
        assert_eq!(
            verdict,
            case.expected.rendered(),
            "the `[safe]` verdict for the loaded {}",
            case.label()
        );
    }
}

#[test]
fn the_eval_route_applies_the_gate_too() {
    // The whole `-eval` route (CLI grammar → evaluator → `call_bif`), which is how a `-eval`
    // program reaches the BIF. The grammar has no `fun M:F/A`, so the image is a binary literal.
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    for name in ["lists", "map", "no_such_fun_xyz", "safe"] {
        rt.intern(name);
    }
    let live = format!(
        "erlang:binary_to_term(<<{}>>, [safe])",
        image_literal(&export_image("lists", "map", 2))
    );
    let term = rt.eval(&live).expect("`lists:map/2` is an active export");
    assert_eq!(
        beam_codec::term::fun_kind_term(term).expect("a fun"),
        FunKind::External
    );

    let dead = format!(
        "erlang:binary_to_term(<<{}>>, [safe])",
        image_literal(&export_image("lists", "no_such_fun_xyz", 1))
    );
    let error = rt.eval(&dead).expect_err("a dead export is refused under [safe]");
    assert_eq!(error.as_atom(), Some("badarg"), "{error:?}");

    // The same dead image without `[safe]` decodes on the `-eval` route as well — the gate is
    // never consulted off the flag.
    let ungated = format!(
        "erlang:binary_to_term(<<{}>>)",
        image_literal(&export_image("lists", "no_such_fun_xyz", 1))
    );
    rt.eval(&ungated)
        .expect("the flag-free decode never asks the gate");
}
