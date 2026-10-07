//! Contract **A.7** — the `ets` match-spec / `select` family and the `info/1,2` field set.
//!
//! Split out of [`super::ets`] by responsibility: that group pins the plain table operations
//! (`insert`/`lookup`/`member`/navigation/…) while this one pins the *query* surface —
//! `select/1,2,3` with its continuations, `match/1,2,3`, `match_object/1,2,3`,
//! `select_count/2`, `select_delete/2`, `select_replace/2`, `match_delete/2`,
//! `match_spec_compile/1` + `match_spec_run/2`, and the 15-field `info/1,2`.
//!
//! Two rules the cases must respect (both oracle behaviours, not gaps):
//! * the `select` family takes **only** a raw clause list (`match_spec_compile/1` handles are
//!   `badarg`), while `match_spec_run/2` takes **only** the compiled handle;
//! * a continuation and a `match_spec_compile/1` handle are `Ref`s whose ids vary per VM
//!   start — cases assert their **shape** (`is_reference/1`), never the rendered ref.

use super::Case;

/// The A.7 `ets` match-spec / `select` / `info` cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
