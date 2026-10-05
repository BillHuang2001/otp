//! Fragmented distribution messages: the `'E'` fragment header, the `'F'`
//! continuation, sender splitting and receiver reassembly.
//!
//! Ground truth: `external.c` (`erts_encode_ext_dist_header_setup`/
//! `_fragment`/`_finalize`), `dist.c` (`ERTS_PREP_DIST_EXT_FRAG_CONT`),
//! `dist.h` `ERTS_DIST_FRAGMENT_SIZE` / `ERTS_DIST_FRAGMENT_HEADER_SIZE`.
//! Implementation pending.
