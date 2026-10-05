//! Byte-exact ground-truth vectors produced by the system oracle.
//!
//! Each vector asserts **both** that our decoder yields the expected term and that
//! our encoder reproduces the exact `term_to_binary` bytes. Regenerate a vector
//! with the system Erlang on `PATH` (OTP 29.x / erts-17.1):
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n", [term_to_binary(T)])' -s init stop
//! ```
//!
//! — one term per vector; paste the printed decimal byte list.
//!
//! Seeds (verified in-tree):
//! `{ok,17}` → `131,104,2,119,2,111,107,97,17`;
//! `[1,2,3]` → `131,107,0,3,1,2,3`;
//! `"abc"` → `131,107,0,3,97,98,99`;
//! `#{a=>1}` → `131,116,0,0,0,1,119,1,97,97,1`;
//! `3.14` → `131,70,64,9,30,184,81,235,133,31`;
//! `true` → `131,119,4,116,114,117,101`;
//! `[]` → `131,106`;
//! `12345678901234567890123` → `131,110,10,0,203,68,66,113,118,78,182,66,157,2`.
//!
//! `<<"hi">>` → `131,109,0,0,0,2,104,105` is kept as the documented *unsupported*
//! case (binary terms return [`beam_codec::unsupported`] in Milestone 1).
//!
//! The vectors themselves are added when the encode/decode bodies land; this file
//! currently holds only the generation recipe and the seed set.
