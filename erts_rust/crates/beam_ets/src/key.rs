//! Object and key handling — the DB key rules of an ETS object.
//!
//! An ETS *object* is a tuple; its *key* is element `keypos` (1-based) of that
//! tuple. These helpers validate an object and extract its key, mirroring the
//! key handling of `../../erts/emulator/beam/erl_db_util.c`. They also expose the
//! tuple payload slice used by `select` predicates.

use beam_common::Result;
use beam_term::prelude::*;

/// The key of an ETS *object*: element `keypos` (1-based) of the tuple.
///
/// Fails with `badarg` when `object` is not a tuple, when its arity is less than
/// `keypos`, or when `keypos` is `0`.
pub fn key_of(object: Eterm, keypos: usize) -> Result<Eterm> {
    unimplemented!("M1: implementation pending")
}

/// Whether `object` is a valid ETS object for `keypos` (non-erroring form of
/// [`key_of`]).
pub fn is_valid_object(object: Eterm, keypos: usize) -> bool {
    unimplemented!("M1: implementation pending")
}

/// The element payload slice of the tuple `obj` (the header word excluded),
/// handed to `select` predicates.
///
/// # Safety
///
/// `obj` must be a live tuple term (its boxed header word must be a valid,
/// aligned arity value). The returned slice borrows storage owned by the
/// table (or the original heap) and stays valid only for as long as that
/// storage does — the caller must not outlive it.
pub unsafe fn object_elements<'a>(obj: Eterm) -> &'a [Eterm] {
    unimplemented!("M1: implementation pending")
}
