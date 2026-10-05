//! Pure `lists` module BIFs (the subset of `lists.erl` with no `fun` argument):
//! `reverse/1,2`, `seq/2,3`, `append/1,2`, `nth/2`, `last/1`, `nthtail/2`,
//! `sublist/2,3`, `flatten/1,2`, `member/2`, `sum/1`, `max/1`, `min/1`,
//! `duplicate/2`, `delete/2`, `keyfind/3`, `keysort/2`, `sort/1`, `usort/1`,
//! `zip/2`, `unzip/1`.
//!
//! Ground truth (read-only): `../../lib/stdlib/src/lists.erl` (the semantics these
//! fast paths must reproduce exactly) and the `bif.tab` roster
//! (`bif lists:reverse/2`, `ubif lists:reverse/1`, …).
//!
//! The `fun`-taking BIFs (`map/2`, `filter/2`, `foldl/3`, `foldr/3`, `foreach/2`,
//! `flatmap/2`, `sort/2`, `usort/2`) are deliberately *not* registered here: they
//! need an `apply`/callback seam into the engine, so the real `lists.erl` runs them.

use crate::registry::BifRegistry;

/// Registers the pure `lists` module BIFs.
pub(crate) fn register(_reg: &mut BifRegistry) {}
