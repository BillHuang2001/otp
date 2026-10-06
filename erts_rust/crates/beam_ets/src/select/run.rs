//! Table-independent match-spec runner: `ets:match_spec_run/2`.
//!
//! The oracle's `ets:match_spec_run/2` takes a spec **already compiled** by
//! `ets:match_spec_compile/1` (passing a raw clause list is `badarg`, verified
//! against OTP 29 / erts-17.1). This crate has no separate compiled-spec *term*,
//! so [`match_spec_run`] accepts the **raw** spec term and compiles it internally
//! with [`super::compile`]; the `beam_bifs` bridge maps `ets:match_spec_compile/1`
//! onto the same compiled [`super::MatchSpec`].
//!
//! It runs the spec against each element of a list **in order**, keeping the body
//! value of every element whose first matching clause succeeds and dropping the
//! rest — the list analogue of `ets:select/2` (order preserved, unlike a table's
//! unspecified iteration order).
//!
//! (Skeleton: the body is filled in by the match-spec-run task.)

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

/// `ets:match_spec_run(List, CompiledSpec)` — run a raw match-spec `spec` against
/// each element of the proper list `list`, in order, returning the body value of
/// every matching element (non-matching elements are dropped).
pub fn match_spec_run(
    list: Eterm,
    spec: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Result<Vec<Eterm>> {
    let _ = (list, spec, atoms, heap);
    Err(crate::error::unsupported("match_spec_run: not yet implemented"))
}
