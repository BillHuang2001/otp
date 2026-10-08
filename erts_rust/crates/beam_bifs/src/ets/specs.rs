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

//! The **compiled match-spec** family of the `ets` bridge:
//! `ets:match_spec_compile/1`, `ets:match_spec_run/2` and
//! `ets:match_spec_run_r/3`.
//!
//! `ets:match_spec_compile/1` compiles a raw match-spec clause list into an opaque
//! term — the oracle returns a `#Ref<…>` (verified: `is_reference/1` is true for
//! it) — and `ets:match_spec_run/2` runs that compiled spec against a list of
//! terms, returning the body value of every matching element.
//!
//! `ets:match_spec_run_r/3` is the same run with an **accumulator**: it *prepends*
//! each match onto `Acc` in list order, so the result is the reversed matching
//! prefix with `Acc` as its tail (C `ets_match_spec_run_r_3`, `erl_db.c:4610`):
//! `ets:match_spec_run_r([{1,a},{2,b},{1,c}], MS, [])` → `[c,a]` and
//! `(…, MS, [acc])` → `[c,a,acc]` (verified), while the forward
//! `ets:match_spec_run/2` gives `[a,c]`.
//!
//! # Registration (the key is shared with the runtime-coupled family)
//!
//! `src/runtime/other.rs` registers `ets:match_spec_run_r/3` on the `RuntimeHooks`
//! seam, and `crate::runtime::register` runs **last** in `with_core_bifs`, so the
//! registry's entry for that key is the seam stub, not [`match_spec_run_r_3`] (both
//! answer `unsupported_bif` with no hook installed, so nothing observable changes
//! today). The BIF-level tests below therefore call the function **directly**; a
//! registry-dispatch test would exercise the stub.
//!
//! # The compiled-spec term is not a select-family spec (verified)
//!
//! The two families are deliberately asymmetric, and the asymmetry was probed on
//! the system `erl` (OTP 29 / erts-17.1):
//!
//! * the **select** family (`ets:select/1,2,3`, `ets:select_count/2`,
//!   `ets:select_delete/2`, `ets:select_replace/2`) takes only a **raw** clause
//!   list; a compiled term from `ets:match_spec_compile/1` is `badarg` there;
//! * `ets:match_spec_run/2` takes only a **compiled** term; a raw clause list is
//!   `badarg` there.
//!
//! Both halves fall out of the implementations without a special case: the select
//! family runs its argument through [`beam_ets::compile`] (which rejects a ref
//! thing because it is not a list), while [`match_spec_run_2`] and
//! [`match_spec_run_r_3`] resolve their second argument through
//! [`TableRegistry::spec_raw`] (which rejects a list because `ref_thing_id` fails
//! on it).
//!
//! # The compiled-spec handle (M1)
//!
//! ERTS compiles a spec into an internal `db_match_program` and returns a magic
//! reference to it. Milestone-1 has no `make_ref` in `beam_term`, so
//! [`match_spec_compile_1`] builds the same frozen 4-word ref thing the table
//! handles use ([`crate::access::make_ref_thing`]) and keys a
//! [`TableRegistry::insert_spec`] entry on its id — an id allocated from the
//! disjoint [`super::SPEC_ID_BASE`] namespace, so a compiled-spec ref never
//! compares `=:=` to a table handle or a continuation. A handle the registry does
//! not know (a table handle, a continuation, a forged id) is `badarg`, as is a
//! non-reference.
//!
//! # Why the *raw* spec term is what the registry stores
//!
//! [`beam_ets::match_spec_run`] takes the **raw** clause list and compiles it with
//! [`beam_ets::compile`] internally — it exposes no entry point that consumes a
//! pre-compiled [`beam_ets::MatchSpec`]. The registry therefore stores the raw spec
//! term the caller passed to `match_spec_compile/1`; the compile in that BIF is
//! what *validates* the spec (a malformed one is `badarg`, exactly as the oracle's
//! `ets:match_spec_compile(bad)` and `ets:match_spec_compile([{bad}])` fail), and
//! its compiled result is not retained because nothing in the crate can consume it
//! (a stored but never-read field would be a silent lie about what the runtime
//! holds, not a faithful compiled program).
//!
//! **Heap-lifetime caveat (M1).** The stored term is a live `Eterm` on the
//! *compiling caller's* heap, not a GC-managed magic reference: `beam_term` has no
//! reference-object machinery in M1 and this BIF cannot allocate it on any other
//! process's behalf. That is sound for the M1 runtime because a process heap is a
//! never-relocating bump arena that persists across the BIF calls of the same
//! process — the same assumption the `ets` table handles and the `ets:select`
//! continuations already rely on — so a **same-process** `match_spec_run/2` reads
//! exactly the spec that was compiled. It is *not* sound across processes the way
//! ERTS's owned program is: a compiled spec handed to another process would read
//! the compiling process's heap. This is a documented, deliberate M1 substitution,
//! not a silently wrong result for the same-process use the runtime makes.

use beam_common::Result;
use beam_ets::compile;
use beam_term::Eterm;

use crate::access::{is_nil, make_ref_thing, ref_thing_id};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

/// Registers the compiled match-spec family.
pub(super) fn register(reg: &mut BifRegistry) {
    reg.register("ets", "match_spec_compile", 1, match_spec_compile_1);
    reg.register("ets", "match_spec_run", 2, match_spec_run_2);
    reg.register("ets", "match_spec_run_r", 3, match_spec_run_r_3);
}

/// `ets:match_spec_compile(MatchSpec)` — compile a raw match-spec clause list into
/// an opaque compiled-spec term (a `#Ref<…>`, as the oracle returns).
///
/// A malformed spec is `badarg` (verified: `ets:match_spec_compile(bad)` and
/// `ets:match_spec_compile([{bad}])` both raise `badarg`).
fn match_spec_compile_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let raw = args[0];
    // Compiling validates the spec (a malformed one is `badarg`); the compiled form
    // is not retained — `beam_ets::match_spec_run` recompiles the raw term, which
    // is therefore what the registry stores (see the module docs).
    compile(raw, ctx.atoms())?;
    let handle = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.insert_spec(raw)
    };
    make_ref_thing(ctx.heap(), handle)
}

/// `ets:match_spec_run(List, CompiledSpec)` — run `CompiledSpec` (a term returned
/// by `ets:match_spec_compile/1`) against each element of the proper list `List`,
/// returning the body value of every element whose clause matches; non-matching
/// elements are dropped, so the result is a list of at most `length(List)` terms.
///
/// `CompiledSpec` **must** be a compiled-spec handle: a raw clause list is `badarg`
/// (verified — the direct opposite of the select family), as is any ref thing the
/// registry does not know (a table handle, a continuation, a forged id) and any
/// non-reference. `List` must be a proper list; anything else is `badarg` (raised
/// by [`beam_ets::match_spec_run`]).
fn match_spec_run_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[1])?;
    let raw = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.spec_raw(handle).ok_or_else(badarg)?
    };
    // `beam_ets::match_spec_run` needs the atom table and the caller heap at once;
    // the context lends them through two conflicting borrows, so it is given a
    // clone of the atom table (identical indices, read-only — the `select_2`
    // pattern).
    let atoms = ctx.atoms().clone();
    let results = beam_ets::match_spec_run(args[0], raw, &atoms, ctx.heap())?;
    // Every result aliases the caller's list, the stored spec or a term freshly
    // built on the caller's heap, so they only need to be consed into a list (the
    // oracle's `ets:match_spec_run/2` likewise shares structure with its input).
    ctx.list(&results)
}

/// `ets:match_spec_run_r(List, CompiledSpec, Acc)` — run `CompiledSpec` (a term
/// returned by `ets:match_spec_compile/1`) against each element of the proper list
/// `List` and **prepend** the body value of every matching element onto `Acc`, in
/// list order (C `ets_match_spec_run_r_3`, `erl_db.c:4610-4651`).
///
/// The accumulator threads through the C loop as `ret = CONS(hp, res, ret)`, so the
/// result is the *reversed* matching prefix with `Acc` as its tail — the oracle's
/// `ets:match_spec_run_r([{1,a},{2,b},{1,c}], MS, [])` is `[c,a]` and
/// `(…, [acc])` is `[c,a,acc]` (verified), while the forward `ets:match_spec_run/2`
/// is `[a,c]`. `List == []` returns `Acc` **verbatim** (the C's `BIF_ARG_1 == NIL`
/// early return before the loop).
///
/// `CompiledSpec` **must** be a compiled-spec handle: a raw clause list is `badarg`
/// (the direct opposite of the select family), as is any ref thing the registry
/// does not know (a table handle, a continuation, a forged id) and any
/// non-reference. `List` must be a proper list or `[]`; anything else is `badarg`.
fn match_spec_run_r_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = ref_thing_id(args[1])?;
    let raw = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.spec_raw(handle).ok_or_else(badarg)?
    };
    // The C's `BIF_ARG_1 == NIL` early return: no element is visited, so the
    // accumulator comes back untouched (it is *not* copied — the oracle returns the
    // very term it was handed).
    if is_nil(args[0]) {
        return Ok(args[2]);
    }
    // The `match_spec_run_2` pattern: `beam_ets::match_spec_run` needs the atom
    // table and the caller heap at once, and those are two conflicting context
    // borrows, so it is given a clone of the atom table (identical indices,
    // read-only).
    let atoms = ctx.atoms().clone();
    let results = beam_ets::match_spec_run(args[0], raw, &atoms, ctx.heap())?;
    // `results` is in list order; consing each onto the accumulator reverses it, so
    // the last match ends up at the head and `Acc` at the tail — the C
    // `CONS(hp, res, ret)` fold. Every result aliases the caller's list, the stored
    // spec or a fresh term on the caller's heap, so nothing needs copying.
    let mut acc = args[2];
    for r in results {
        acc = ctx.cons(r, acc)?;
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    use crate::ets::{TableRegistry, SPEC_ID_BASE};

    /// The atom names the specs and the assertions below need.
    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["a", "b", "c", "z", "foo", "$1", "$2", "set"]);
        atoms
    }

    fn parts() -> (Heap, AtomTable, TableRegistry) {
        (
            Heap::new(65536).expect("test heap"),
            seeded_atoms(),
            TableRegistry::new(),
        )
    }

    /// `[{{'$1','$2'}, [], [{{'$1',b}}]}]` — matches every 2-tuple and replaces it
    /// with `{Key, b}`, keeping the key at position 1.
    fn replace_spec(ctx: &mut BifContext<'_>) -> Eterm {
        let v1 = ctx.atom("$1").expect("seeded");
        let v2 = ctx.atom("$2").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let head = ctx.tuple(&[v1, v2]).expect("fits");
        let guards = ctx.list(&[]).expect("fits");
        // The `{{ … }}` construction form: a 1-tuple holding the tuple to build.
        let constructed = ctx.tuple(&[v1, b]).expect("fits");
        let body_item = ctx.tuple(&[constructed]).expect("fits");
        let body = ctx.list(&[body_item]).expect("fits");
        let clause = ctx.tuple(&[head, guards, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    /// The 2-tuple `{Key, Value}` built on `ctx`.
    fn pair(ctx: &mut BifContext<'_>, key: &str, value: i64) -> Eterm {
        let k = ctx.atom(key).expect("seeded");
        let v = ctx.int(value).expect("fits");
        ctx.tuple(&[k, v]).expect("fits")
    }

    /// The 2-tuple `{Key, Name}` with a **small-int** key, built on `ctx`.
    fn keyed_pair(ctx: &mut BifContext<'_>, key: i64, name: &str) -> Eterm {
        let k = ctx.int(key).expect("fits");
        let v = ctx.atom(name).expect("seeded");
        ctx.tuple(&[k, v]).expect("fits")
    }

    /// `[{{1,'$1'}, [], ['$1']}]` — matches every `{1, Value}` 2-tuple and yields
    /// `Value` (the oracle probe the `match_spec_run_r/3` test below pins). The body
    /// is the **bare** variable `'$1'` (the oracle's own form: `['$1']`, not
    /// `[{'$1'}]` — the 1-tuple wrapper means "construct this tuple", which a
    /// variable is not).
    fn pick_first_spec(ctx: &mut BifContext<'_>) -> Eterm {
        let one = ctx.int(1).expect("fits");
        let v1 = ctx.atom("$1").expect("seeded");
        let head = ctx.tuple(&[one, v1]).expect("fits");
        let guards = ctx.list(&[]).expect("fits");
        let body = ctx.list(&[v1]).expect("fits");
        let clause = ctx.tuple(&[head, guards, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    fn is_raise_atom(err: &BeamError, reason: &'static str) -> bool {
        matches!(err, BeamError::Raise(raise) if raise.reason == beam_common::ExitReason::atom(reason))
    }

    fn nil(ctx: &mut BifContext<'_>) -> Eterm {
        ctx.list(&[]).expect("fits")
    }

    /// `$ERTS_ORACLE` if it names an existing file, else `erl` on `$PATH` — the same
    /// resolution `tools/differential` and `tests/checksum.rs` use.
    fn resolve_oracle() -> Option<std::path::PathBuf> {
        if let Some(value) = std::env::var_os("ERTS_ORACLE") {
            let program = std::path::PathBuf::from(&value);
            if program.is_file() {
                return Some(program);
            }
            eprintln!(
                "[beam_bifs ets specs] $ERTS_ORACLE='{}' is not an existing file — \
                 treating it as no oracle.",
                program.display()
            );
            return None;
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("erl"))
            .find(|candidate| candidate.is_file())
    }

    #[test]
    fn match_spec_compile_returns_a_fresh_ref_in_the_spec_namespace() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let spec = replace_spec(&mut ctx);
        let handle = match_spec_compile_1(&mut ctx, &[spec]).expect("ets:match_spec_compile/1");
        let id = ref_thing_id(handle).expect("a compiled spec must be a ref thing");
        assert!(
            id >= SPEC_ID_BASE,
            "compiled-spec id {id} is below the spec namespace base"
        );

        // Every compile gets a fresh handle.
        let spec = replace_spec(&mut ctx);
        let second = match_spec_compile_1(&mut ctx, &[spec]).expect("ets:match_spec_compile/1");
        assert_ne!(ref_thing_id(second).expect("ref"), id);

        // A non-list spec is `badarg` (the oracle's `ets:match_spec_compile(bad)`).
        let one = ctx.int(1).expect("fits");
        assert!(matches!(
            match_spec_compile_1(&mut ctx, &[one]),
            Err(BeamError::BadArg)
        ));

        // So is a list whose clause is not a 3-tuple (verified with `[{bad}]`).
        let bad_clause = ctx.tuple(&[one]).expect("fits");
        let bad_spec = ctx.list(&[bad_clause]).expect("fits");
        assert!(matches!(
            match_spec_compile_1(&mut ctx, &[bad_spec]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn match_spec_run_serves_a_compiled_handle_and_rejects_everything_else() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let spec = replace_spec(&mut ctx);
        let handle = match_spec_compile_1(&mut ctx, &[spec]).expect("ets:match_spec_compile/1");

        // The body values of the matching elements, in list order (`{a,1}` ⇒
        // `{a,b}`, `{z,9}` ⇒ `{z,b}`) — verified against the oracle.
        let a1 = pair(&mut ctx, "a", 1);
        let z9 = pair(&mut ctx, "z", 9);
        let input = ctx.list(&[a1, z9]).expect("fits");
        let got = match_spec_run_2(&mut ctx, &[input, handle]).expect("ets:match_spec_run/2");
        let a_b = ctx.tuple(&[ctx.atom("a").unwrap(), ctx.atom("b").unwrap()]).expect("fits");
        let z_b = ctx.tuple(&[ctx.atom("z").unwrap(), ctx.atom("b").unwrap()]).expect("fits");
        let expected = ctx.list(&[a_b, z_b]).expect("fits");
        assert!(
            beam_term::compare::eq(got, expected, &atoms),
            "ets:match_spec_run/2 projection mismatch"
        );

        // The handle stays live across calls — the oracle's compiled spec is
        // reusable and the registry does not consume the entry.
        let a2 = pair(&mut ctx, "a", 2);
        let second_input = ctx.list(&[a2]).expect("fits");
        let got = match_spec_run_2(&mut ctx, &[second_input, handle]).expect("reuse");
        let a_b = ctx.tuple(&[ctx.atom("a").unwrap(), ctx.atom("b").unwrap()]).expect("fits");
        let expected = ctx.list(&[a_b]).expect("fits");
        assert!(
            beam_term::compare::eq(got, expected, &atoms),
            "a compiled-spec handle must be reusable"
        );

        // A non-matching element is dropped: an atom is not a 2-tuple, so the head
        // never matches it.
        let foo = ctx.atom("foo").expect("seeded");
        let nonmatching = ctx.list(&[foo]).expect("fits");
        let got = match_spec_run_2(&mut ctx, &[nonmatching, handle]).expect("run no match");
        let empty = nil(&mut ctx);
        assert!(beam_term::compare::eq(got, empty, &atoms), "no-match must be []");

        // The empty list yields the empty list.
        let empty_list = nil(&mut ctx);
        let got = match_spec_run_2(&mut ctx, &[empty_list, handle]).expect("run empty");
        let empty = nil(&mut ctx);
        assert!(beam_term::compare::eq(got, empty, &atoms));

        // A **raw** clause list as the spec is `badarg`: the oracle requires the
        // compiled handle here, the opposite of the select family (verified).
        let raw = replace_spec(&mut ctx);
        assert!(matches!(
            match_spec_run_2(&mut ctx, &[input, raw]),
            Err(BeamError::BadArg)
        ));

        // A ref thing the registry does not know (a forged handle) is `badarg`.
        let forged = make_ref_thing(ctx.heap(), SPEC_ID_BASE + 4096).expect("forged ref");
        assert!(matches!(
            match_spec_run_2(&mut ctx, &[input, forged]),
            Err(BeamError::BadArg)
        ));

        // A non-reference spec is `badarg`.
        let one = ctx.int(1).expect("fits");
        assert!(matches!(
            match_spec_run_2(&mut ctx, &[input, one]),
            Err(BeamError::BadArg)
        ));

        // A non-list first argument is `badarg` (raised by `beam_ets::match_spec_run`).
        assert!(matches!(
            match_spec_run_2(&mut ctx, &[one, handle]),
            Err(BeamError::BadArg)
        ));
        // An improper list is `badarg` too.
        let improper = ctx.cons(a1, foo).expect("fits");
        assert!(matches!(
            match_spec_run_2(&mut ctx, &[improper, handle]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn match_spec_run_r_prepends_each_match_onto_the_accumulator() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let spec = pick_first_spec(&mut ctx);
        let handle = match_spec_compile_1(&mut ctx, &[spec]).expect("ets:match_spec_compile/1");

        // `{1,a}` matches, `{2,b}` does not (the head's first element is `1`), and
        // `{1,c}` matches — the objective's oracle input. The body is `'$1'`, so a
        // match yields the tuple's **second element** (the atom `a`/`c`), not the
        // whole tuple.
        let t_a = keyed_pair(&mut ctx, 1, "a");
        let t_b = keyed_pair(&mut ctx, 2, "b");
        let t_c = keyed_pair(&mut ctx, 1, "c");
        let input = ctx.list(&[t_a, t_b, t_c]).expect("fits");
        let a = ctx.atom("a").expect("seeded");
        let c = ctx.atom("c").expect("seeded");

        // Oracle: `ets:match_spec_run_r([{1,a},{2,b},{1,c}], MS, [])` => `[c,a]`: the
        // C conses each match onto the accumulator, so the result is the matching
        // prefix **reversed**.
        let empty_acc = nil(&mut ctx);
        let got = match_spec_run_r_3(&mut ctx, &[input, handle, empty_acc]).expect("run_r");
        let expected = ctx.list(&[c, a]).expect("fits");
        assert!(
            beam_term::compare::eq(got, expected, &atoms),
            "ets:match_spec_run_r/3 with an empty accumulator must be the reversed prefix"
        );

        // Oracle: `(…, MS, [acc])` => `[c,a,acc]` — the oracle's accumulator is the
        // one-element list `[acc]`; `[foo]` plays that role here.
        let foo = ctx.atom("foo").expect("seeded");
        let acc = ctx.list(&[foo]).expect("fits");
        let got = match_spec_run_r_3(&mut ctx, &[input, handle, acc]).expect("run_r");
        let expected = ctx.list(&[c, a, foo]).expect("fits");
        assert!(
            beam_term::compare::eq(got, expected, &atoms),
            "ets:match_spec_run_r/3 must leave Acc as the tail"
        );

        // The tail is that very list term, not a copy: the two cons cells the fold
        // added wrap `Acc` verbatim.
        assert!(crate::access::is_list(got), "a match conses onto Acc");
        let tail = crate::access::cdr(crate::access::cdr(got));
        assert_eq!(tail, acc, "Acc must be the list tail verbatim");

        // The forward run of the same list is in the *opposite* order (`[a,c]`).
        let got = match_spec_run_2(&mut ctx, &[input, handle]).expect("run");
        let expected = ctx.list(&[a, c]).expect("fits");
        assert!(
            beam_term::compare::eq(got, expected, &atoms),
            "ets:match_spec_run/2 must stay in list order"
        );

        // Oracle: `ets:match_spec_run_r([], MS, [acc])` => `[acc]` — the C's
        // `BIF_ARG_1 == NIL` early return hands `Acc` back **verbatim** (the same
        // term, not a copy).
        let nil_list = nil(&mut ctx);
        let acc = ctx.list(&[ctx.atom("foo").expect("seeded")]).expect("fits");
        let got = match_spec_run_r_3(&mut ctx, &[nil_list, handle, acc]).expect("run_r []");
        assert_eq!(got, acc, "ets:match_spec_run_r/3 of [] must return Acc verbatim");
    }

    #[test]
    fn match_spec_run_r_rejects_a_bad_list_or_a_bad_spec_with_badarg() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let spec = pick_first_spec(&mut ctx);
        let handle = match_spec_compile_1(&mut ctx, &[spec]).expect("ets:match_spec_compile/1");
        let t_a = keyed_pair(&mut ctx, 1, "a");
        let input = ctx.list(&[t_a]).expect("fits");
        let acc = nil(&mut ctx);
        let one = ctx.int(1).expect("fits");
        let foo = ctx.atom("foo").expect("seeded");

        // A non-list first argument is `badarg` (verified with `x`).
        assert!(matches!(
            match_spec_run_r_3(&mut ctx, &[one, handle, acc]),
            Err(BeamError::BadArg)
        ));
        // An improper list is `badarg` too (verified with `[{1,a}|x]`).
        let improper = ctx.cons(t_a, foo).expect("fits");
        assert!(matches!(
            match_spec_run_r_3(&mut ctx, &[improper, handle, acc]),
            Err(BeamError::BadArg)
        ));

        // A **raw** clause list as the spec is `badarg`: the oracle requires the
        // compiled handle here (verified with `[{{1,'$1'},[],['$1']}]`).
        let raw = pick_first_spec(&mut ctx);
        assert!(matches!(
            match_spec_run_r_3(&mut ctx, &[input, raw, acc]),
            Err(BeamError::BadArg)
        ));
        // A non-reference spec is `badarg` (verified with `17`).
        assert!(matches!(
            match_spec_run_r_3(&mut ctx, &[input, one, acc]),
            Err(BeamError::BadArg)
        ));
        // A ref thing the registry does not know (a forged handle) is `badarg`.
        let forged = make_ref_thing(ctx.heap(), SPEC_ID_BASE + 4096).expect("forged ref");
        assert!(matches!(
            match_spec_run_r_3(&mut ctx, &[input, forged, acc]),
            Err(BeamError::BadArg)
        ));

        // The spec is resolved before the accumulator is touched, so the `[]` early
        // return still validates the handle (verified: `match_spec_run_r([], 17, [])`
        // is `badarg`).
        let nil_list = nil(&mut ctx);
        assert!(matches!(
            match_spec_run_r_3(&mut ctx, &[nil_list, one, acc]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn match_spec_run_r_reports_a_typed_unsupported_bif_without_a_registry() {
        let (mut heap, atoms, mut reg) = parts();
        // A valid handle has to be built first: the BIF decodes its handle before
        // consulting the registry, so the missing-registry path is only reached with
        // a real ref-thing handle.
        let handle = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
            let spec = pick_first_spec(&mut ctx);
            match_spec_compile_1(&mut ctx, &[spec]).expect("compile")
        };

        // No `.with_tables(...)`: a typed `unsupported_bif` raise rather than a
        // fabricated empty-accumulator result.
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let list = nil(&mut ctx);
        let acc = ctx.list(&[ctx.atom("foo").expect("seeded")]).expect("fits");
        let err = match_spec_run_r_3(&mut ctx, &[list, handle, acc]).expect_err("no registry");
        assert!(
            is_raise_atom(&err, "unsupported_bif"),
            "ets:match_spec_run_r/3 without a registry: {err:?}"
        );
    }

    #[test]
    fn match_spec_run_r_cross_checks_the_live_oracle() {
        let Some(oracle) = resolve_oracle() else {
            eprintln!();
            eprintln!("=========== beam_bifs ets specs: live oracle SKIPPED ===========");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
            eprintln!("===============================================================");
            eprintln!();
            return;
        };

        // The oracle re-derives the exact expectations this module asserts, so the
        // literals cannot silently drift from the real runtime.
        let eval = "MS = ets:match_spec_compile([{{1,'$1'},[],['$1']}]), \
                    R1 = ets:match_spec_run_r([{1,a},{2,b},{1,c}], MS, []), \
                    R2 = ets:match_spec_run_r([{1,a},{2,b},{1,c}], MS, [acc]), \
                    R3 = ets:match_spec_run_r([], MS, [acc]), \
                    R4 = ets:match_spec_run([{1,a},{2,b},{1,c}], MS), \
                    IsBadArg = fun(F) -> case catch F() of {'EXIT',{badarg,_}} -> true; \
                                                       _ -> false end end, \
                    B1 = IsBadArg(fun() -> ets:match_spec_run_r([{1,a}|x], MS, []) end), \
                    B2 = IsBadArg(fun() -> ets:match_spec_run_r([{1,a}], \
                                        [{{1,'$1'},[],['$1']}], []) end), \
                    io:format(\"~w~n\", [{R1 =:= [c,a], R2 =:= [c,a,acc], R3 =:= [acc], \
                                         R4 =:= [a,c], B1, B2}]), \
                    halt().";
        let output = std::process::Command::new(&oracle)
            .arg("-noshell")
            .arg("-eval")
            .arg(eval)
            .arg("-s")
            .arg("init")
            .arg("stop")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
        assert!(
            output.status.success(),
            "the oracle '{}' exited with {:?}\n--- stderr ---\n{}",
            oracle.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8(output.stdout).expect("the oracle printed UTF-8");
        assert_eq!(
            stdout.trim(),
            "{true,true,true,true,true,true}",
            "the live erts-17.1 oracle disagrees with the pinned run_r/3 expectations"
        );
    }

    #[test]
    fn both_bifs_report_a_typed_unsupported_bif_without_a_registry() {
        let (mut heap, atoms, mut reg) = parts();
        // A valid handle has to be built first: the compiled-spec BIFs decode their
        // handle before consulting the registry, so the missing-registry path is
        // only reached with a real ref-thing handle.
        let handle = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
            let spec = replace_spec(&mut ctx);
            match_spec_compile_1(&mut ctx, &[spec]).expect("compile")
        };

        // No `.with_tables(...)`: both BIFs report a typed `unsupported_bif` raise
        // rather than a fabricated compiled spec.
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let spec = replace_spec(&mut ctx);
        let err = match_spec_compile_1(&mut ctx, &[spec]).expect_err("no registry installed");
        assert!(
            is_raise_atom(&err, "unsupported_bif"),
            "ets:match_spec_compile/1 without a registry: {err:?}"
        );
        let list = nil(&mut ctx);
        let err =
            match_spec_run_2(&mut ctx, &[list, handle]).expect_err("no registry installed");
        assert!(
            is_raise_atom(&err, "unsupported_bif"),
            "ets:match_spec_run/2 without a registry: {err:?}"
        );
    }

    #[test]
    fn this_module_registers_its_three_keys() {
        let reg = BifRegistry::with_core_bifs();
        assert!(reg.contains("ets", "match_spec_compile", 1));
        assert!(reg.contains("ets", "match_spec_run", 2));
        // `ets:match_spec_run_r/3` is registered **here** too, but `src/runtime/`
        // registers the same key through its `RuntimeHooks` stub and
        // `crate::runtime::register` runs last in `with_core_bifs`, so the registry's
        // entry for that key is the stub — which is why this module's own tests call
        // `match_spec_run_r_3` directly instead of dispatching through the registry.
        assert!(reg.contains("ets", "match_spec_run_r", 3));
        // The registry is the *composition* of this module's roster with the parent
        // module's, so an `ets:new/2` key is present too.
        assert!(reg.contains("ets", "new", 2));
    }
}
