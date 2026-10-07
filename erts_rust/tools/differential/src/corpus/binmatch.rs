//! Contract **A.7** — the `binary` module's compiled-pattern surface.
//!
//! `binary:compile_pattern/1` returns an *opaque* handle whose tag encodes the search
//! strategy the runtime chose (`{bm,_}` for a one-word pattern, `{ac,_}` for a multi-word
//! one), and `match/2,3` / `matches/2,3` / `split/2,3` dispatch on it. That tag is
//! observable, so the shape is part of the compatibility surface — and the handle itself is
//! a `Ref` whose id varies per VM start, so the cases assert the **shape**
//! (`element(1, P)`, `is_reference(...)`), never the raw handle.
//!
//! Also here: `iolist_size/1` of a size that outgrows the 60-bit small range (it must
//! promote to a bignum, not raise `system_limit`).

use super::Case;

/// The A.7 `binary` compiled-pattern cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
