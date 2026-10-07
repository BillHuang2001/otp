//! Contract **A.7** — the `~p` printer and the `short` float rendering.
//!
//! Two renderings a program's output depends on and that a runtime must reproduce
//! byte-for-byte:
//!
//! * the `~p`/`~P` printer (the `io_lib_pretty` port): nested containers, re-flow at the
//!   line limit, the explicit depth argument of `~P`/`~W`, and the printable-latin1 rule
//!   that decides whether a list/binary renders as a string;
//! * `float_to_list/2` / `float_to_binary/2` with the `short` option (the C
//!   `sys_double_to_chars_short` port), whose notation choice (`0.0001` vs `1.0e-5`) and
//!   exact-tie rule are observable.
//!
//! The probes print a **flattened** rendering (`lists:flatten/1` of `io_lib:format/2`), so
//! the compared stdout line is the exact character sequence a program would see.

use super::Case;

/// The A.7 printer / float-formatting cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
