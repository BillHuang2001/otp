//! Exhaustive encode → decode round-trips plus negative cases.
//!
//! Planned corpus (property-style over boundary values): boundary integers
//! (small/bignum/sign), nested and improper lists, tuples at every arity boundary,
//! every atom tag (Latin-1 and UTF-8, short and long), floats including `±0`,
//! `±inf` and `NaN` bit patterns, maps of 0/1/32 pairs, pids/ports/references.
//!
//! Planned negative cases: truncation at every offset, unknown tag, over-long atom
//! (`BeamError::SystemLimit`), `SMALL_BIG_EXT`/`LARGE_BIG_EXT` with more than
//! `BIG_ARITY_MAX` digits, and trailing bytes (`Decoder::finish` → `BeamError::BadArg`).
//!
//! Every malformed input must yield an error — never a panic, never a silently
//! truncated or mis-encoded term.
//!
//! The tests are added when the encode/decode bodies land; this file currently
//! holds only the plan.
