//! The distribution challenge/response authentication: MD5 digest and nonce.
//!
//! Owns the concrete, byte-exact implementation of the handshake digest
//! `md5(cookie ++ decimal(challenge))` (ground truth: `dist.c` `gen_digest`,
//! `lib/kernel/src/dist_util.erl` `gen_digest/2`, `lib/erl_interface/src/connect/ei_connect.c`
//! `gen_digest`) behind the [`crate::handshake::ChallengeDigest`] seam, plus the
//! challenge nonce source. Implementation pending.
