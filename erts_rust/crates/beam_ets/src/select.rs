//! Selection: the `select` primitive and the M1 match-spec subset.
//!
//! [`SelectPredicate`] is the code-level primitive [`crate::Table::select_all`]
//! consumes: it receives an object's tuple **element slice** and returns whether
//! to keep the object. [`MatchSpec`] is the *data* form (`ets:select/2` match
//! specs) that [`compile`] decodes into its M1 subset — head = tuple of literals
//! / `'_'` (wildcard) / `'$N'` (bind element `N`), **empty guards only**, body =
//! tuple of literals / `'$N'` / `'$_'` (whole object). Anything outside the subset
//! is rejected with `badarg` (see the crate `CONTEXT.md`, Known Issues).
//!
//! # Why this module reads term words
//!
//! Decoding a spec term means reading its tuples and lists. The frozen
//! representation (`beam_term`) exposes exactly those accessors (`is_tuple`,
//! `tuple_size`, `tuple_element`, `car`, `cdr`) as `unsafe fn`, because "this word
//! is a live term of the right shape" is a caller invariant the word itself
//! cannot re-check. Reading the spec term therefore needs `unsafe`; every block
//! below carries a `// SAFETY:` rationale, and the reads never go beyond the
//! caller-owned spec term.

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;

/// The code-level select primitive: receives an object's tuple element slice and
/// returns whether the object is kept.
pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a;

/// One element of a match-clause head pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchElement {
    /// `'_'` — matches any element.
    Wildcard,
    /// A literal element that must compare equal.
    Literal(Eterm),
    /// `'$N'` — matches any element and binds it to variable `N`.
    Bind(u32),
}

/// The head pattern of a [`MatchClause`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchPattern {
    /// A bare `'_'` head: matches any object.
    Wildcard,
    /// A tuple head: matches a tuple whose elements match the per-element
    /// patterns.
    Tuple(Vec<MatchElement>),
}

/// The guard of a [`MatchClause`].
///
/// M1 supports only the empty guard; guards (`andalso`/`orelse`, `is_*` tests,
/// `element/2`, …) are not part of the subset and cause [`compile`] to reject
/// the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchGuard {
    /// No guard — the clause matches whenever its head matches.
    Empty,
}

/// One element of a match-clause body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchBodyElement {
    /// A literal element.
    Literal(Eterm),
    /// `'$N'` — the value bound to variable `N` by the head.
    Bind(u32),
    /// `'$_'` — the whole matched object.
    WholeObject,
}

/// The body (result template) of a [`MatchClause`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchBody {
    /// A bare `'$_'` body: the whole matched object is the result.
    WholeObject,
    /// A tuple body: the result is built from the given elements.
    Tuple(Vec<MatchBodyElement>),
}

/// One match-spec clause: a head pattern, a guard and a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchClause {
    /// The head pattern the object is matched against.
    pub head: MatchPattern,
    /// The guard (M1: empty only).
    pub guard: MatchGuard,
    /// The body producing the result for a matched object.
    pub body: MatchBody,
}

/// A compiled match spec: an ordered list of clauses; the first matching clause
/// wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSpec {
    /// The clauses, in order.
    pub clauses: Vec<MatchClause>,
}

// ---------------------------------------------------------------------------
// Term decoding
//
// The low-level helpers read words of the caller-owned spec term through
// `beam_term`'s `unsafe` accessors (see the module docs).
// ---------------------------------------------------------------------------

/// The name of an atom term, or `None` when `term` is not an atom (or the passed
/// table does not know it). Non-erroring: an unknown atom is treated as a
/// literal by the callers.
fn atom_name<'a>(term: Eterm, atoms: &'a AtomTable) -> Option<&'a str> {
    if is_atom(term) {
        atoms.name(atom_val(term))
    } else {
        None
    }
}

/// Decode a match-spec variable atom.
///
/// Returns `Ok(None)` when `name` is not `$`-sigilled (the caller then treats the
/// term as a literal), `Ok(Some(n))` for a well-formed `'$n'`, and an error for a
/// `$`-sigilled atom that is not a `'$n'` (`'$'`, `'$x'`, `'$1x'`, an
/// out-of-range `n`, …): `$` is reserved for variables, so a malformed one is
/// `badarg` rather than a literal.
fn bind_index(name: Option<&str>) -> Result<Option<u32>> {
    let Some(rest) = name.and_then(|name| name.strip_prefix('$')) else {
        return Ok(None);
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return Err(badarg());
    }
    rest.parse::<u32>().map(Some).map_err(|_| badarg())
}

/// The elements of a tuple term, or `None` when `term` is not a tuple.
fn tuple_elements(term: Eterm) -> Option<Vec<Eterm>> {
    // SAFETY: `is_tuple` inspects only the header word of a boxed term and checks
    // the boxed/arityval shape first, so it is sound for any value word (it never
    // dereferences a non-boxed, e.g. immediate, term).
    if !unsafe { is_tuple(term) } {
        return None;
    }
    // SAFETY: `is_tuple` established that `term` is a live tuple whose header word
    // is a valid arityval, so its arity is readable.
    let arity = unsafe { tuple_size(term) };
    // SAFETY: every `i` in `0..arity` is `< tuple_size(term)`, which is exactly
    // `tuple_element`'s precondition; the tuple lives in the caller-owned spec.
    Some((0..arity).map(|i| unsafe { tuple_element(term, i) }).collect())
}

/// The elements of a proper list term, or `None` when `term` is not a proper list
/// (a non-cons value, or an improper list).
fn list_elements(term: Eterm) -> Option<Vec<Eterm>> {
    let mut elements = Vec::new();
    let mut cursor = term;
    while is_list(cursor) {
        // SAFETY: `is_list(cursor)` established that `cursor` is a live cons cell,
        // so its two `[CAR, CDR]` words are readable. The spec term is caller-owned
        // and stays live for the whole call, so `cursor` never dangles.
        unsafe {
            elements.push(car(cursor));
            cursor = cdr(cursor);
        }
    }
    // A proper list terminates in `NIL`; anything else is an improper list.
    is_nil(cursor).then_some(elements)
}

/// Decode one match-clause head *element*.
fn decode_head_element(term: Eterm, atoms: &AtomTable) -> Result<MatchElement> {
    let name = atom_name(term, atoms);
    if name == Some("_") {
        return Ok(MatchElement::Wildcard);
    }
    match bind_index(name)? {
        Some(index) => Ok(MatchElement::Bind(index)),
        None => Ok(MatchElement::Literal(term)),
    }
}

/// Decode one match-clause body *element*.
fn decode_body_element(term: Eterm, atoms: &AtomTable) -> Result<MatchBodyElement> {
    let name = atom_name(term, atoms);
    if name == Some("$_") {
        return Ok(MatchBodyElement::WholeObject);
    }
    match bind_index(name)? {
        Some(index) => Ok(MatchBodyElement::Bind(index)),
        None => Ok(MatchBodyElement::Literal(term)),
    }
}

/// Decode a match-clause head pattern: the atom `'_'`, or a tuple of element
/// patterns.
fn decode_head(head: Eterm, atoms: &AtomTable) -> Result<MatchPattern> {
    if atom_name(head, atoms) == Some("_") {
        return Ok(MatchPattern::Wildcard);
    }
    match tuple_elements(head) {
        Some(elements) => {
            let mut pattern = Vec::with_capacity(elements.len());
            for element in elements {
                pattern.push(decode_head_element(element, atoms)?);
            }
            Ok(MatchPattern::Tuple(pattern))
        }
        None => Err(badarg()),
    }
}

/// Decode a match-clause body template: the atom `'$_'`, or a tuple of body
/// elements.
fn decode_body(body: Eterm, atoms: &AtomTable) -> Result<MatchBody> {
    if atom_name(body, atoms) == Some("$_") {
        return Ok(MatchBody::WholeObject);
    }
    match tuple_elements(body) {
        Some(elements) => {
            let mut template = Vec::with_capacity(elements.len());
            for element in elements {
                template.push(decode_body_element(element, atoms)?);
            }
            Ok(MatchBody::Tuple(template))
        }
        None => Err(badarg()),
    }
}

/// Decode one `{Head, Guards, Body}` clause.
fn decode_clause(clause: Eterm, atoms: &AtomTable) -> Result<MatchClause> {
    let elements = tuple_elements(clause).ok_or_else(badarg)?;
    if elements.len() != 3 {
        return Err(badarg());
    }
    // M1 supports the empty guard only; any guard expression is outside the subset.
    if !is_nil(elements[1]) {
        return Err(badarg());
    }
    Ok(MatchClause {
        head: decode_head(elements[0], atoms)?,
        guard: MatchGuard::Empty,
        body: decode_body(elements[2], atoms)?,
    })
}

/// Decode the M1 match-spec subset from its term form into a [`MatchSpec`].
///
/// The term form is an Erlang list of `{Head, Guards, Body}` clauses; `Guards`
/// must be the empty list. Fails with `badarg` for any construct outside the
/// subset (a non-list spec, a clause that is not a 3-tuple, a non-empty guard,
/// a malformed `'$'` variable atom, or an unsupported head/body shape).
pub fn compile(spec: Eterm, atoms: &AtomTable) -> Result<MatchSpec> {
    let clauses = list_elements(spec).ok_or_else(badarg)?;
    let mut decoded = Vec::with_capacity(clauses.len());
    for clause in clauses {
        decoded.push(decode_clause(clause, atoms)?);
    }
    Ok(MatchSpec { clauses: decoded })
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// A table seeded with the atoms the specs below reference.
    fn table() -> AtomTable {
        let mut atoms = AtomTable::new();
        for name in ["_", "$_", "$1", "$2", "ok", "true"] {
            atoms.intern(name);
        }
        atoms
    }

    /// The term of an atom interned by [`table`].
    fn atom(atoms: &AtomTable, name: &str) -> Eterm {
        make_atom(atoms.lookup(name).expect("atom interned by `table`"))
    }

    /// A proper list of `elements`, built on `heap`.
    fn list(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
        let mut acc = NIL;
        for element in elements.iter().rev() {
            acc = heap.cons(*element, acc).expect("test heap has room");
        }
        acc
    }

    #[test]
    fn wildcard_head_with_empty_guard_and_whole_object_body() {
        let atoms = table();
        let mut heap = Heap::new(64);

        let clause = heap
            .tuple(&[atom(&atoms, "_"), NIL, atom(&atoms, "$_")])
            .expect("test heap has room");
        let spec = list(&mut heap, &[clause]);

        let compiled = compile(spec, &atoms).expect("valid spec");
        assert_eq!(compiled.clauses.len(), 1);
        let clause = &compiled.clauses[0];
        assert_eq!(clause.head, MatchPattern::Wildcard);
        assert_eq!(clause.guard, MatchGuard::Empty);
        assert_eq!(clause.body, MatchBody::WholeObject);
    }

    #[test]
    fn tuple_head_decodes_literals_wildcards_and_binds() {
        let atoms = table();
        let mut heap = Heap::new(64);

        let head = heap
            .tuple(&[make_small(42), atom(&atoms, "_"), atom(&atoms, "$1")])
            .expect("test heap has room");
        let clause = heap
            .tuple(&[head, NIL, atom(&atoms, "$_")])
            .expect("test heap has room");
        let spec = list(&mut heap, &[clause]);

        let compiled = compile(spec, &atoms).expect("valid spec");
        assert_eq!(
            compiled.clauses[0].head,
            MatchPattern::Tuple(vec![
                MatchElement::Literal(make_small(42)),
                MatchElement::Wildcard,
                MatchElement::Bind(1),
            ])
        );
        assert_eq!(compiled.clauses[0].guard, MatchGuard::Empty);
        assert_eq!(compiled.clauses[0].body, MatchBody::WholeObject);
    }

    #[test]
    fn tuple_body_decodes_literals_binds_and_whole_object() {
        let atoms = table();
        let mut heap = Heap::new(64);

        let body = heap
            .tuple(&[atom(&atoms, "ok"), atom(&atoms, "$2"), atom(&atoms, "$_")])
            .expect("test heap has room");
        let clause = heap
            .tuple(&[atom(&atoms, "_"), NIL, body])
            .expect("test heap has room");
        let spec = list(&mut heap, &[clause]);

        let compiled = compile(spec, &atoms).expect("valid spec");
        assert_eq!(
            compiled.clauses[0].body,
            MatchBody::Tuple(vec![
                MatchBodyElement::Literal(atom(&atoms, "ok")),
                MatchBodyElement::Bind(2),
                MatchBodyElement::WholeObject,
            ])
        );
    }

    #[test]
    fn clauses_are_decoded_in_order() {
        let atoms = table();
        let mut heap = Heap::new(64);

        let head = heap
            .tuple(&[atom(&atoms, "$1"), atom(&atoms, "$2")])
            .expect("test heap has room");
        let first = heap
            .tuple(&[head, NIL, atom(&atoms, "$_")])
            .expect("test heap has room");
        let second = heap
            .tuple(&[atom(&atoms, "_"), NIL, atom(&atoms, "$_")])
            .expect("test heap has room");
        let spec = list(&mut heap, &[first, second]);

        let compiled = compile(spec, &atoms).expect("valid spec");
        assert_eq!(compiled.clauses.len(), 2);
        assert_eq!(
            compiled.clauses[0].head,
            MatchPattern::Tuple(vec![
                MatchElement::Bind(1),
                MatchElement::Bind(2),
            ])
        );
        assert_eq!(compiled.clauses[1].head, MatchPattern::Wildcard);
    }

    #[test]
    fn empty_spec_is_a_valid_empty_match_spec() {
        let atoms = table();
        let compiled = compile(NIL, &atoms).expect("empty spec is valid");
        assert!(compiled.clauses.is_empty());
    }

    #[test]
    fn rejects_non_list_spec() {
        let atoms = table();
        assert!(matches!(
            compile(make_small(1), &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            compile(atom(&atoms, "_"), &atoms),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn rejects_clause_that_is_not_a_three_tuple() {
        let atoms = table();
        let mut heap = Heap::new(64);

        // A 2-tuple clause.
        let two = heap
            .tuple(&[atom(&atoms, "_"), atom(&atoms, "$_")])
            .expect("test heap has room");
        let spec = list(&mut heap, &[two]);
        assert!(matches!(compile(spec, &atoms), Err(BeamError::BadArg)));

        // A non-tuple clause.
        let spec = list(&mut heap, &[make_small(0)]);
        assert!(matches!(compile(spec, &atoms), Err(BeamError::BadArg)));
    }

    #[test]
    fn rejects_non_empty_guard() {
        let atoms = table();
        let mut heap = Heap::new(64);

        let guard = list(&mut heap, &[atom(&atoms, "true")]);
        let clause = heap
            .tuple(&[atom(&atoms, "_"), guard, atom(&atoms, "$_")])
            .expect("test heap has room");
        let spec = list(&mut heap, &[clause]);
        assert!(matches!(compile(spec, &atoms), Err(BeamError::BadArg)));
    }

    #[test]
    fn rejects_unsupported_head_and_body_shapes() {
        let atoms = table();
        let mut heap = Heap::new(64);

        // A head that is neither `'_'` nor a tuple.
        let clause = heap
            .tuple(&[atom(&atoms, "ok"), NIL, atom(&atoms, "$_")])
            .expect("test heap has room");
        let spec = list(&mut heap, &[clause]);
        assert!(matches!(compile(spec, &atoms), Err(BeamError::BadArg)));

        // A body that is neither `'$_'` nor a tuple.
        let clause = heap
            .tuple(&[atom(&atoms, "_"), NIL, make_small(7)])
            .expect("test heap has room");
        let spec = list(&mut heap, &[clause]);
        assert!(matches!(compile(spec, &atoms), Err(BeamError::BadArg)));
    }

    #[test]
    fn rejects_malformed_dollar_atoms() {
        let mut atoms = table();
        let mut heap = Heap::new(128);

        for bad in ["$", "$x", "$1x", "$_$"] {
            let bad_atom = atoms.intern(bad);
            let head = heap.tuple(&[make_atom(bad_atom)]).expect("test heap has room");
            let clause = heap
                .tuple(&[head, NIL, atom(&atoms, "$_")])
                .expect("test heap has room");
            let spec = list(&mut heap, &[clause]);
            assert!(
                matches!(compile(spec, &atoms), Err(BeamError::BadArg)),
                "`{bad}` should be rejected in a head element"
            );

            let body = heap.tuple(&[make_atom(bad_atom)]).expect("test heap has room");
            let clause = heap
                .tuple(&[atom(&atoms, "_"), NIL, body])
                .expect("test heap has room");
            let spec = list(&mut heap, &[clause]);
            assert!(
                matches!(compile(spec, &atoms), Err(BeamError::BadArg)),
                "`{bad}` should be rejected in a body element"
            );
        }
    }

    #[test]
    fn rejects_improper_list_spec() {
        let atoms = table();
        let mut heap = Heap::new(64);

        // A clause list whose tail is not `NIL`.
        let clause = heap
            .tuple(&[atom(&atoms, "_"), NIL, atom(&atoms, "$_")])
            .expect("test heap has room");
        let improper = heap.cons(clause, make_small(0)).expect("test heap has room");
        assert!(matches!(compile(improper, &atoms), Err(BeamError::BadArg)));
    }
}
