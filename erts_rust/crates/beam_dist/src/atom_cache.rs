//! The distribution atom-cache / segment machinery on the payload side.
//!
//! Builds and applies the 8-segment × 256-slot distribution atom cache:
//! applying a decoded [`crate::dist_header::DistHeader`] to a
//! [`beam_codec::AtomCache`] (receive side) and tracking outbound segment
//! rotation to produce [`crate::dist_header::AtomCacheRef`]s (send side).
//! Implementation pending.
