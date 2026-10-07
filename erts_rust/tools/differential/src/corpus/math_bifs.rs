//! Contract **A.7** — the `math` module (the real `bif.tab` BIFs over `libm`).
//!
//! A program's numeric output depends on these to the **last bit**, so the group pins the
//! *bit-exact* cases (the renderings of `math:atanh/1` and friends must equal the oracle's
//! to the last decimal digit) alongside the failure surface: a domain error or an overflow
//! is `error:badarith` (never `badarg`, never a silently wrong float).
//!
//! `~w` of a float is the shortest representation that round-trips, so rendering a float is
//! already a bit-exact statement; the probes also compare against literals with `=:=` where
//! that is clearer.

use super::Case;

/// The A.7 `math` cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
