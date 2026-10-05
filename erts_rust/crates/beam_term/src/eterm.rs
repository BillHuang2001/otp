//! The [`Eterm`] word type — the one machine word every BEAM term occupies.
//!
//! `Eterm` is a `#[repr(transparent)]` newtype over `usize` and is therefore
//! layout- and pass-identical to a NIF's `ERL_NIF_TERM` (both are one machine
//! word). `beam_abi` transmutes between them; no other crate may redefine this
//! type (workspace `CONTEXT.md` §2).

use crate::immediate::NIL;
use crate::ops::THE_NON_VALUE;

/// A single BEAM term word.
///
/// The default value is [`NIL`], matching BEAM's "blank register = NIL"
/// initialisation convention.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Eterm(usize);

impl Eterm {
    /// The raw machine word, tags included.
    #[inline]
    pub const fn raw(self) -> usize {
        self.0
    }

    /// Wrap a raw machine word. The caller is responsible for the tag bits.
    #[inline]
    pub const fn from_raw(raw: usize) -> Eterm {
        Eterm(raw)
    }

    /// The empty list `[]` (`NIL`).
    #[inline]
    pub const fn nil() -> Eterm {
        NIL
    }

    /// The magic non-value (`THE_NON_VALUE`), used as a function error return and
    /// "absent value" indicator.
    #[inline]
    pub const fn non_value() -> Eterm {
        THE_NON_VALUE
    }
}

impl Default for Eterm {
    #[inline]
    fn default() -> Eterm {
        NIL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::TAG_IMMED2_NIL;

    #[test]
    fn raw_round_trip() {
        for raw in [0usize, 1, 0x3B, 0xF, 0xDEAD_BEEF, usize::MAX] {
            assert_eq!(Eterm::from_raw(raw).raw(), raw);
        }
    }

    #[test]
    fn default_is_nil() {
        assert_eq!(Eterm::default(), NIL);
        assert_eq!(Eterm::default().raw(), TAG_IMMED2_NIL);
    }

    #[test]
    fn nil_and_non_value_constructors() {
        assert_eq!(Eterm::nil(), NIL);
        assert_eq!(Eterm::non_value(), THE_NON_VALUE);
    }

    #[test]
    fn is_one_word() {
        assert_eq!(core::mem::size_of::<Eterm>(), core::mem::size_of::<usize>());
        assert_eq!(
            core::mem::align_of::<Eterm>(),
            core::mem::align_of::<usize>()
        );
    }
}
